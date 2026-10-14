//! Finding a model's GGUF weights in Ollama's on-disk store, by the name
//! Ollama knows it by (`qwen3:8b`, `user/model:tag`, `host/ns/model:tag`).
//!
//! The store is Ollama's own layout: a manifest per tag under
//! `manifests/<registry>/<namespace>/<model>/<tag>`, listing layers by
//! digest, and the layers under `blobs/sha256-<hex>`. The weights are the
//! layer of media type `application/vnd.ollama.image.model`. Only files are
//! read; Ollama itself is not asked, so it need not be running, and nothing
//! is loaded.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Ollama's defaults for names that leave them out.
const DEFAULT_REGISTRY: &str = "registry.ollama.ai";
const DEFAULT_NAMESPACE: &str = "library";
const DEFAULT_TAG: &str = "latest";
const MODEL_MEDIA_TYPE: &str = "application/vnd.ollama.image.model";

/// Where Ollama keeps models: `OLLAMA_MODELS` when set, else its defaults
/// for a user install and for the Linux system service.
#[must_use]
pub fn stores() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("OLLAMA_MODELS").filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(dir)];
    }
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        dirs.push(Path::new(&home).join(".ollama/models"));
    }
    dirs.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
    dirs
}

/// `name` split the way Ollama splits it: `(registry, namespace, model, tag)`.
/// `None` for something that cannot be a model name (a path, for example).
#[must_use]
pub fn parse_name(name: &str) -> Option<(String, String, String, String)> {
    if name.is_empty() || name.starts_with(['.', '/', '~']) || name.contains("..") {
        return None;
    }
    let (path, tag) = match name.rsplit_once(':') {
        // A colon inside the registry part (`host:port/...`) is not a tag.
        Some((p, t)) if !t.contains('/') => (p, t),
        _ => (name, DEFAULT_TAG),
    };
    let parts: Vec<&str> = path.split('/').collect();
    let (registry, namespace, model) = match parts.as_slice() {
        [model] => (DEFAULT_REGISTRY, DEFAULT_NAMESPACE, *model),
        [namespace, model] => (DEFAULT_REGISTRY, *namespace, *model),
        [registry, namespace, model] => (*registry, *namespace, *model),
        _ => return None,
    };
    let valid = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
    };
    [registry, namespace, model, tag]
        .iter()
        .all(|s| valid(s))
        .then(|| {
            (
                registry.to_string(),
                namespace.to_string(),
                model.to_string(),
                tag.to_string(),
            )
        })
}

/// The weights file of model `name` in `store`.
pub fn weights_in(store: &Path, name: &str) -> Result<PathBuf, String> {
    let (registry, namespace, model, tag) =
        parse_name(name).ok_or_else(|| format!("{name:?} is not an Ollama model name"))?;
    let manifest_path = store
        .join("manifests")
        .join(registry)
        .join(namespace)
        .join(model)
        .join(tag);
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let digest = manifest["layers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|l| l["mediaType"] == MODEL_MEDIA_TYPE)
        .and_then(|l| l["digest"].as_str())
        .ok_or_else(|| format!("{}: no model layer", manifest_path.display()))?;
    let hex = digest
        .strip_prefix("sha256:")
        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("{}: unexpected digest {digest}", manifest_path.display()))?;
    let blob = store.join("blobs").join(format!("sha256-{hex}"));
    if blob.is_file() {
        Ok(blob)
    } else {
        Err(format!("{} is listed but missing", blob.display()))
    }
}

/// Every model name the stores have manifests for, in the short form
/// Ollama shows (`qwen3:8b`, `user/model:tag`, `host/ns/model:tag`),
/// sorted. An absent store lists nothing; one that cannot be read is an
/// error.
pub fn list() -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for store in stores() {
        out.extend(list_in(&store)?);
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// [`list`] for one store.
pub fn list_in(store: &Path) -> Result<Vec<String>, String> {
    let dirs = |d: &Path| -> Result<Vec<String>, String> {
        match std::fs::read_dir(d) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(format!("{}: {e}", d.display())),
            Ok(entries) => entries
                .map(|e| {
                    e.map(|e| e.file_name().to_string_lossy().into_owned())
                        .map_err(|e| format!("{}: {e}", d.display()))
                })
                .collect(),
        }
    };
    let manifests = store.join("manifests");
    let mut out = Vec::new();
    for registry in dirs(&manifests)? {
        for namespace in dirs(&manifests.join(&registry))? {
            let models = manifests.join(&registry).join(&namespace);
            for model in dirs(&models)? {
                let tags = models.join(&model);
                for tag in dirs(&tags)? {
                    if !tags.join(&tag).is_file() {
                        continue;
                    }
                    let name = match (registry.as_str(), namespace.as_str()) {
                        (DEFAULT_REGISTRY, DEFAULT_NAMESPACE) => format!("{model}:{tag}"),
                        (DEFAULT_REGISTRY, ns) => format!("{ns}/{model}:{tag}"),
                        (r, ns) => format!("{r}/{ns}/{model}:{tag}"),
                    };
                    if parse_name(&name).is_some() {
                        out.push(name);
                    }
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The weights file of model `name`, from the first store that has it.
pub fn weights(name: &str) -> Result<PathBuf, String> {
    let mut tried = Vec::new();
    for store in stores() {
        match weights_in(&store, name) {
            Ok(path) => return Ok(path),
            Err(e) => tried.push(e),
        }
    }
    Err(format!(
        "{name} not found in Ollama's store ({}); set OLLAMA_MODELS if it lives elsewhere",
        tried.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_split_the_way_ollama_splits_them() {
        let p = |n| parse_name(n).map(|(r, ns, m, t)| format!("{r}/{ns}/{m}/{t}"));
        assert_eq!(
            p("qwen3:8b").unwrap(),
            "registry.ollama.ai/library/qwen3/8b"
        );
        assert_eq!(
            p("qwen3").unwrap(),
            "registry.ollama.ai/library/qwen3/latest"
        );
        assert_eq!(
            p("me/vision:q4").unwrap(),
            "registry.ollama.ai/me/vision/q4"
        );
        assert_eq!(p("hf.co/org/model:Q8_0").unwrap(), "hf.co/org/model/Q8_0");
        for not_a_name in ["./m.gguf", "/m.gguf", "~/m", "a/../b", "a/b/c/d", ""] {
            assert_eq!(p(not_a_name), None, "{not_a_name}");
        }
    }

    #[test]
    fn finds_the_model_layer_by_media_type() {
        let store = std::env::temp_dir().join(format!("hearth-ollama-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&store);
        let manifests = store.join("manifests/registry.ollama.ai/library/qwen3");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::create_dir_all(store.join("blobs")).unwrap();
        std::fs::write(
            manifests.join("8b"),
            r#"{"schemaVersion":2,"layers":[
                {"mediaType":"application/vnd.ollama.image.template","digest":"sha256:aa"},
                {"mediaType":"application/vnd.ollama.image.model","digest":"sha256:beef"}]}"#,
        )
        .unwrap();
        std::fs::write(store.join("blobs/sha256-beef"), b"GGUF").unwrap();
        assert_eq!(
            weights_in(&store, "qwen3:8b").unwrap(),
            store.join("blobs/sha256-beef")
        );
        assert!(weights_in(&store, "qwen3:14b").is_err());
        let other = store.join("manifests/registry.ollama.ai/user/m");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("q4"), "{}").unwrap();
        assert_eq!(list_in(&store).unwrap(), ["qwen3:8b", "user/m:q4"]);
        assert!(list_in(&store.join("absent")).unwrap().is_empty());
        std::fs::remove_dir_all(store).unwrap();
    }
}
