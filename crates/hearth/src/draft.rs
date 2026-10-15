//! The draft model of speculative decoding (llama.cpp `-md`): its weights
//! and its own KV cache are on the accelerator beside the main model's.

use std::path::{Path, PathBuf};

use hearth_sense::kv::CacheType;

/// A draft model as a command line or `hearth fit` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub model: PathBuf,
    /// Its context; the main model's when not given (llama.cpp's default).
    pub ctx: Option<u64>,
    /// Its KV cache types; llama.cpp's f16 default when not given.
    pub cache: Option<(CacheType, CacheType)>,
}

/// The value of the last of `names` in `words`, as `-x V` or `--x=V`.
fn last_value(words: &[String], names: &[&str]) -> Option<String> {
    words.iter().enumerate().rev().find_map(|(i, w)| {
        names.iter().find_map(|n| {
            if w == n {
                words.get(i + 1).filter(|v| !v.starts_with('-')).cloned()
            } else {
                w.strip_prefix(n)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::to_string)
            }
        })
    })
}

/// The draft model a llama-server command line loads, if any.
#[must_use]
pub fn from_args(words: &[String]) -> Option<Draft> {
    let model = last_value(words, &["-md", "--model-draft"])?;
    let ctx = last_value(words, &["-cd", "--ctx-size-draft"]).and_then(|v| v.parse().ok());
    let k =
        last_value(words, &["-ctkd", "--cache-type-k-draft"]).and_then(|v| CacheType::parse(&v));
    let v =
        last_value(words, &["-ctvd", "--cache-type-v-draft"]).and_then(|v| CacheType::parse(&v));
    Some(Draft {
        model: PathBuf::from(model),
        ctx,
        cache: k.map(|k| (k, v.unwrap_or(k))),
    })
}

/// What the draft holds: `(weights, KV at its context)`, from its own file.
pub fn bytes(draft: &Draft, main_ctx: u64) -> Result<(u64, u64), String> {
    let (_, weights, geometry) = crate::model_files::weights_and_geometry(Path::new(&draft.model))
        .map_err(|e| format!("draft model {}: {e}", draft.model.display()))?;
    let (k, v) = draft.cache.unwrap_or((CacheType::F16, CacheType::F16));
    let kv = geometry.kv_bytes(draft.ctx.unwrap_or(main_ctx), k, v, None);
    Ok((weights, kv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_and_its_settings_from_the_command_line() {
        let words = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(from_args(&words("llama-server -m big.gguf -c 8192")), None);
        assert_eq!(
            from_args(&words(
                "llama-server -m big.gguf -md small.gguf -cd 4096 -ctkd q8_0"
            )),
            Some(Draft {
                model: PathBuf::from("small.gguf"),
                ctx: Some(4096),
                cache: Some((CacheType::Q8_0, CacheType::Q8_0)),
            })
        );
        assert_eq!(
            from_args(&words("llama-server --model-draft=s.gguf")).map(|d| (d.ctx, d.cache)),
            Some((None, None))
        );
    }
}
