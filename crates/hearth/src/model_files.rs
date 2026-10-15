//! A model's files as `fit` reads them: a GGUF and its shards, or an MLX
//! or safetensors directory with its `config.json`, found by path, by store
//! name or by Ollama name.

use std::path::{Path, PathBuf};

use hearth_sense::gguf;
use hearth_sense::kv::{CacheType, Geometry};

/// The path given, or the Ollama model of that name.
pub(crate) fn from_name(model: &std::path::Path) -> Result<PathBuf, String> {
    if model.exists() {
        return Ok(model.to_path_buf());
    }
    let name = model.to_string_lossy();
    // org/repo names live in the Hugging Face cache or LM Studio first:
    // that is where `llama-server -hf` and LM Studio put them.
    if let Some(found) = hearth_runtime::model_stores::find(&name) {
        return found;
    }
    if hearth_runtime::ollama_store::parse_name(&name).is_none() {
        return Err(format!("{}: no such file", model.display()));
    }
    hearth_runtime::ollama_store::weights(&name).map_err(|e| {
        format!(
            "{} is not a file, and as an Ollama model: {e}",
            model.display()
        )
    })
}

/// The cache type of a model directory whose runtime caches keys and
/// values in the model's own dtype (MLX, Transformers): `torch_dtype`, or
/// `dtype` in newer configs, of the language model.
pub(crate) fn dtype_cache_type(model: &Path) -> Option<CacheType> {
    let text = std::fs::read_to_string(model.join("config.json")).ok()?;
    let config: serde_json::Value = serde_json::from_str(&text).ok()?;
    [&config["text_config"], &config]
        .iter()
        .flat_map(|c| [&c["torch_dtype"], &c["dtype"]])
        .find_map(serde_json::Value::as_str)
        .and_then(|d| match d {
            "bfloat16" => Some(CacheType::Bf16),
            "float16" => Some(CacheType::F16),
            "float32" => Some(CacheType::F32),
            _ => None,
        })
}

/// The model's files, their total size, and its cache geometry: from the
/// GGUF header, or for a directory with `config.json` (MLX, safetensors)
/// from that config and the `.safetensors` files beside it.
pub(crate) fn weights_and_geometry(
    model: &std::path::Path,
) -> Result<(Vec<PathBuf>, u64, Geometry), String> {
    let size = |files: &[PathBuf]| {
        files
            .iter()
            .map(|f| {
                std::fs::metadata(f)
                    .map(|m| m.len())
                    .map_err(|e| format!("{}: {e}", f.display()))
            })
            .sum::<Result<u64, String>>()
    };
    if model.is_dir() {
        let config_path = model.join("config.json");
        let text = std::fs::read_to_string(&config_path).map_err(|e| {
            format!(
                "{}: {e}; a model directory needs config.json",
                config_path.display()
            )
        })?;
        let config: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", config_path.display()))?;
        let mut files = safetensors_weights(model)?;
        files.sort();
        files.dedup();
        if files.is_empty() {
            return Err(format!("{}: no .safetensors weights", model.display()));
        }
        let geometry = Geometry::from_hf_config(&config).map_err(|e| e.to_string())?;
        return Ok((files.clone(), size(&files)?, geometry));
    }
    let files = gguf::model_files(model);
    let weights = size(&files)?;
    let header = gguf::read_header(&files[0]).map_err(|e| e.to_string())?;
    let geometry = Geometry::from_header(&header).map_err(|e| e.to_string())?;
    Ok((files, weights, geometry))
}

/// The weight files of a safetensors model directory: those its
/// `model.safetensors.index.json` maps tensors to when it has one (so a
/// `consolidated.safetensors` beside the shards is not counted twice),
/// else every `.safetensors` file but a `LoRA` adapter's `adapters.safetensors`.
pub(crate) fn safetensors_weights(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let index = dir.join("model.safetensors.index.json");
    if index.is_file() {
        let text =
            std::fs::read_to_string(&index).map_err(|e| format!("{}: {e}", index.display()))?;
        let json: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", index.display()))?;
        let map = json["weight_map"]
            .as_object()
            .ok_or_else(|| format!("{}: no weight_map", index.display()))?;
        return map
            .values()
            .map(|v| {
                // Shards sit beside the index; a name is never a path.
                v.as_str()
                    .filter(|n| Path::new(n).file_name().is_some_and(|f| f == *n))
                    .map(|n| dir.join(n))
                    .ok_or_else(|| format!("{}: bad shard name {v}", index.display()))
            })
            .collect();
    }
    Ok(std::fs::read_dir(dir)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("safetensors"))
                && p.file_name().is_some_and(|n| n != "adapters.safetensors")
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safetensors_weights_follow_the_index() {
        let dir = std::env::temp_dir().join(format!("hearth-st-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "a.safetensors",
            "b.safetensors",
            "consolidated.safetensors",
            "adapters.safetensors",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let names = |v: Vec<PathBuf>| {
            let mut n: Vec<String> = v
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            n.sort();
            n.dedup();
            n
        };
        assert_eq!(
            names(safetensors_weights(&dir).unwrap()),
            ["a.safetensors", "b.safetensors", "consolidated.safetensors"]
        );
        let index = dir.join("model.safetensors.index.json");
        std::fs::write(
            &index,
            r#"{"weight_map":{"x":"a.safetensors","y":"b.safetensors","z":"a.safetensors"}}"#,
        )
        .unwrap();
        assert_eq!(
            names(safetensors_weights(&dir).unwrap()),
            ["a.safetensors", "b.safetensors"]
        );
        std::fs::write(&index, r#"{"weight_map":{"x":"../a.safetensors"}}"#).unwrap();
        assert!(safetensors_weights(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mlx_caches_in_the_model_dtype() {
        let dir = std::env::temp_dir().join(format!("hearth-dtype-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(dtype_cache_type(&dir), None);
        std::fs::write(
            dir.join("config.json"),
            r#"{"text_config":{"dtype":"bfloat16"},"torch_dtype":"float32"}"#,
        )
        .unwrap();
        assert_eq!(dtype_cache_type(&dir), Some(CacheType::Bf16));
        std::fs::write(dir.join("config.json"), r#"{"torch_dtype":"float16"}"#).unwrap();
        assert_eq!(dtype_cache_type(&dir), Some(CacheType::F16));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
