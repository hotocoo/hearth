//! What a running server's command line says about what it loads: its
//! model (`--model`), KV cache types (`-ctk`/`-ctv`) and multimodal
//! projector (`--mmproj`, or the one `-hf` downloads beside the model).

use std::path::{Path, PathBuf};

use hearth_sense::kv::CacheType;

/// The `LLAMA_ARG_*` environment variables llama.cpp reads in place of
/// flags hearth looks at, with the flag each stands for (llama.cpp's
/// common/arg.cpp), and whether it takes a value; a switch is on for the
/// values llama.cpp reads as true.
const ENV_FLAGS: &[(&str, &str, bool)] = &[
    ("LLAMA_ARG_MODEL", "--model", true),
    ("LLAMA_ARG_CTX_SIZE", "--ctx-size", true),
    ("LLAMA_ARG_N_PARALLEL", "--parallel", true),
    ("LLAMA_ARG_CACHE_TYPE_K", "--cache-type-k", true),
    ("LLAMA_ARG_CACHE_TYPE_V", "--cache-type-v", true),
    ("LLAMA_ARG_MMPROJ", "--mmproj", true),
    ("LLAMA_ARG_NO_MMPROJ", "--no-mmproj", false),
    ("LLAMA_ARG_NO_MMPROJ_OFFLOAD", "--no-mmproj-offload", false),
    ("LLAMA_ARG_HF_REPO", "--hf-repo", true),
    ("LLAMA_ARG_MODEL_DRAFT", "--model-draft", true),
    ("LLAMA_ARG_CTX_SIZE_DRAFT", "--ctx-size-draft", true),
    ("LLAMA_ARG_CACHE_TYPE_K_DRAFT", "--cache-type-k-draft", true),
    ("LLAMA_ARG_CACHE_TYPE_V_DRAFT", "--cache-type-v-draft", true),
    ("LLAMA_ARG_N_GPU_LAYERS_DRAFT", "--gpu-layers-draft", true),
];

/// A command line with what its `LLAMA_ARG_*` environment says put in as
/// flags, right after the program: llama.cpp lets a flag on the command
/// line override its variable, and later flags win here too.
pub(crate) fn with_env(args: Vec<String>, env: &[(String, String)]) -> Vec<String> {
    let mut words = args.into_iter();
    let mut out: Vec<String> = words.next().into_iter().collect();
    for (name, flag, takes_value) in ENV_FLAGS {
        let Some((_, value)) = env.iter().find(|(n, _)| n == name) else {
            continue;
        };
        if *takes_value {
            out.extend([(*flag).to_string(), value.clone()]);
        } else if ["1", "true", "on", "enabled"].contains(&value.to_ascii_lowercase().as_str()) {
            out.push((*flag).to_string());
        }
    }
    out.extend(words);
    out
}

/// What a running llama-server was told: its command line, and its
/// `LLAMA_ARG_*` environment where readable.
pub(crate) fn server_words(pid: i32) -> Option<Vec<String>> {
    let args = hearth_sense::process_args(pid).ok()?;
    let env = hearth_sense::process_env(pid, "LLAMA_ARG_").unwrap_or_default();
    Some(with_env(args, &env))
}

/// The value of `--model` (or `--model=VALUE`) on a command line, as
/// `mlx_lm.server` and vLLM take the model they serve.
pub(crate) fn model_arg(args: &[String]) -> Option<String> {
    args.iter().enumerate().find_map(|(i, a)| {
        a.strip_prefix("--model=").map(str::to_string).or_else(|| {
            (a == "--model")
                .then(|| args.get(i + 1).filter(|v| !v.starts_with('-')).cloned())
                .flatten()
        })
    })
}

/// The multimodal projector a llama-server loads onto the accelerator
/// beside its model (`--mmproj`/`-mm FILE`), from its command line. None
/// when it is kept on the CPU (`--no-mmproj-offload`, `--mmproj-device
/// none`) or not loaded (`--no-mmproj`). The last occurrence wins, as in
/// llama.cpp. Only its weights are counted, not the compute it uses to
/// encode images.
/// Whether the command keeps its projector off the accelerator, or does
/// not load one.
fn projector_off_accelerator(args: &[String]) -> bool {
    args.iter().enumerate().any(|(i, a)| {
        a == "--no-mmproj-offload"
            || a == "--no-mmproj"
            || a == "--mmproj-device=none"
            || a == "-mmdev=none"
            || (["--mmproj-device", "-mmdev"].contains(&a.as_str())
                && args.get(i + 1).is_some_and(|v| v == "none"))
    })
}

pub(crate) fn projector_arg(args: &[String]) -> Option<PathBuf> {
    if projector_off_accelerator(args) {
        return None;
    }
    args.iter()
        .enumerate()
        .rev()
        .find_map(|(i, a)| {
            a.strip_prefix("--mmproj=").map(str::to_string).or_else(|| {
                ["--mmproj", "-mm"]
                    .contains(&a.as_str())
                    .then(|| args.get(i + 1).filter(|v| !v.starts_with('-')).cloned())
                    .flatten()
            })
        })
        .map(PathBuf::from)
}

/// See [`server_cache_types`]: only a llama-server's arguments say this.
pub(crate) fn cache_types_from_args(args: &[String]) -> Option<(CacheType, CacheType)> {
    let program = args.first()?;
    if !std::path::Path::new(program)
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains("llama-server"))
    {
        return None;
    }
    // The last occurrence wins, as in llama.cpp; long flags may be spelt
    // with `_`. An absent flag (with its LLAMA_ARG_CACHE_TYPE_* already
    // put in by `with_env` where the environment was readable) is not
    // assumed to mean the f16 default: the type is then unknown.
    let flag = |names: [&str; 3]| -> Option<CacheType> {
        let value = args.iter().enumerate().rev().find_map(|(i, a)| {
            names.iter().find_map(|n| {
                if a == n {
                    args.get(i + 1).cloned()
                } else {
                    a.strip_prefix(&format!("{n}=")).map(str::to_string)
                }
            })
        })?;
        CacheType::parse(&value)
    };
    Some((
        flag(["-ctk", "--cache-type-k", "--cache_type_k"])?,
        flag(["-ctv", "--cache-type-v", "--cache_type_v"])?,
    ))
}

/// What `llama-server -hf org/repo` loads as its projector, unless told
/// not to: the projector downloaded beside the model file, when its folder
/// holds exactly one. `Err` says why none can be named.
pub(crate) fn hf_projector_of(words: &[String], model: &Path) -> Result<Option<PathBuf>, String> {
    let hf = words
        .iter()
        .any(|w| w == "-hf" || w == "--hf-repo" || w.starts_with("--hf-repo="));
    let declined = projector_off_accelerator(words)
        || words
            .iter()
            .any(|w| w == "-mm" || w.starts_with("--mmproj"));
    let Some(dir) = model.parent().filter(|_| hf && !declined) else {
        return Ok(None);
    };
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.to_ascii_lowercase().contains("mmproj"))
                && p.extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
        })
        .collect();
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        several => Err(format!(
            "{} projectors beside the model and -hf loads one of them",
            several.len()
        )),
    }
}

/// The projector a llama-server with command line `words` serving `model`
/// loads onto the accelerator: its `--mmproj`, else what `-hf` loaded.
/// Only a file readable from here.
pub(crate) fn served_projector(words: &[String], model: &Path) -> Option<PathBuf> {
    projector_arg(words)
        .or_else(|| hf_projector_of(words, model).ok().flatten())
        .filter(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_types_come_from_a_llama_server_command_line() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            cache_types_from_args(&args(&[
                "/x/llama-server",
                "-c",
                "8",
                "--cache-type-k",
                "q8_0",
                "-ctv=q4_0"
            ])),
            Some((CacheType::Q8_0, CacheType::Q4_0))
        );
        assert_eq!(cache_types_from_args(&args(&["llama-server"])), None);
        assert_eq!(
            cache_types_from_args(&args(&[
                "llama-server",
                "-ctk",
                "f16",
                "-ctv",
                "f16",
                "-ctk",
                "q8_0",
                "--cache_type_v",
                "q8_0"
            ])),
            Some((CacheType::Q8_0, CacheType::Q8_0)),
            "the last flag wins"
        );
        assert_eq!(cache_types_from_args(&args(&["ollama", "serve"])), None);
        // The environment stands in for an absent flag; a flag overrides it.
        let env = vec![
            ("LLAMA_ARG_CACHE_TYPE_K".to_string(), "q8_0".to_string()),
            ("LLAMA_ARG_CACHE_TYPE_V".to_string(), "q4_0".to_string()),
        ];
        assert_eq!(
            cache_types_from_args(&with_env(args(&["llama-server", "-ctv", "f16"]), &env)),
            Some((CacheType::Q8_0, CacheType::F16))
        );
        let off = vec![("LLAMA_ARG_NO_MMPROJ".to_string(), "0".to_string())];
        assert_eq!(
            with_env(args(&["llama-server"]), &off),
            args(&["llama-server"])
        );
        assert_eq!(
            cache_types_from_args(&args(&["llama-server", "-ctk", "nonsense"])),
            None
        );
    }

    #[test]
    fn the_model_a_server_was_started_with() {
        let line = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            model_arg(&line("python -m mlx_lm.server --model org/m --port 8081")).as_deref(),
            Some("org/m")
        );
        assert_eq!(
            model_arg(&line("vllm serve --model=/m")).as_deref(),
            Some("/m")
        );
        assert_eq!(model_arg(&line("python -m mlx_lm.server --model")), None);
        assert_eq!(model_arg(&line("mlx_lm.server --model --port 8081")), None);
        assert_eq!(
            projector_arg(&line("llama-server -m m.gguf --mmproj a.gguf -mm b.gguf")),
            Some(PathBuf::from("b.gguf"))
        );
        assert_eq!(
            projector_arg(&line("llama-server --mmproj=p.gguf")),
            Some(PathBuf::from("p.gguf"))
        );
        assert_eq!(projector_arg(&line("llama-server -m m.gguf")), None);
        for cpu in [
            "--no-mmproj-offload",
            "-mmdev none",
            "--mmproj-device=none",
            "--no-mmproj",
        ] {
            assert_eq!(
                projector_arg(&line(&format!("llama-server --mmproj p.gguf {cpu}"))),
                None,
                "{cpu}"
            );
        }
        assert_eq!(model_arg(&line("python -m mlx_lm.server")), None);
    }

    #[test]
    fn the_projector_hf_loads_is_the_one_beside_the_model() {
        let dir = std::env::temp_dir().join(format!("hearth-hfproj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let model = dir.join("m-Q6_K.gguf");
        std::fs::write(&model, "").unwrap();
        let words = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        let hf = words("llama-server -hf org/repo -c 4096");
        assert_eq!(hf_projector_of(&hf, &model), Ok(None));
        std::fs::write(dir.join("mmproj-BF16.gguf"), "").unwrap();
        assert_eq!(
            hf_projector_of(&hf, &model),
            Ok(Some(dir.join("mmproj-BF16.gguf")))
        );
        assert_eq!(
            hf_projector_of(&words("llama-server -hf org/repo --no-mmproj"), &model),
            Ok(None)
        );
        assert_eq!(
            hf_projector_of(&words("llama-server -m m-Q6_K.gguf"), &model),
            Ok(None)
        );
        std::fs::write(dir.join("mmproj-F16.gguf"), "").unwrap();
        assert!(hf_projector_of(&hf, &model).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
