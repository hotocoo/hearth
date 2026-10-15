//! The draft model of speculative decoding (llama.cpp `-md`): its weights
//! and its own KV cache are on the accelerator beside the main model's,
//! unless it is kept on the CPU.

use std::path::{Path, PathBuf};

use hearth_sense::kv::CacheType;

/// A draft model as a command line or `hearth fit` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub model: PathBuf,
    /// Its context per slot (llama.cpp `-cd`); unset (or 0) means the main
    /// context split across the slots, as llama.cpp does.
    pub ctx: Option<u64>,
    /// Its KV cache types (`-ctkd`, `-ctvd`), each f16 unless given.
    pub cache: (CacheType, CacheType),
    /// Server slots (`-np`), each with its own draft context.
    pub slots: u64,
}

/// What a command line says about its draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// No draft.
    None,
    /// A draft on the accelerator.
    Draft(Draft),
    /// A draft held where it is not counted, and why.
    NotCounted(String),
}

/// The value of the last occurrence of any of `names`, as `-x V` or
/// `--x=V`: the last one wins, as in llama.cpp, whatever its value.
fn last_value(words: &[String], names: &[&str]) -> Option<String> {
    words.iter().enumerate().rev().find_map(|(i, w)| {
        names.iter().find_map(|n| {
            if w == n {
                Some(words.get(i + 1).cloned().unwrap_or_default())
            } else {
                w.strip_prefix(n)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::to_string)
            }
        })
    })
}

fn number(words: &[String], names: &[&str]) -> Result<Option<u64>, String> {
    last_value(words, names)
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| format!("{}: {v:?} is not a count", names[0]))
        })
        .transpose()
}

fn cache(words: &[String], names: &[&str]) -> Result<CacheType, String> {
    match last_value(words, names) {
        None => Ok(CacheType::F16),
        Some(v) => {
            CacheType::parse(&v).ok_or_else(|| format!("{}: unknown cache type {v}", names[0]))
        }
    }
}

/// The draft model a llama-server command line loads.
pub fn from_args(words: &[String]) -> Result<Found, String> {
    if last_value(words, &["-hfd", "--hf-repo-draft"]).is_some() {
        return Ok(Found::NotCounted(
            "the server's draft comes from -hfd, which names no file here: not counted".into(),
        ));
    }
    let Some(model) = last_value(words, &["-md", "--model-draft"]) else {
        return Ok(Found::None);
    };
    let layers = number(
        words,
        &["-ngld", "--gpu-layers-draft", "--n-gpu-layers-draft"],
    )?;
    let device = last_value(words, &["-devd", "--device-draft"]);
    if layers == Some(0) || device.as_deref() == Some("none") {
        return Ok(Found::NotCounted(format!(
            "the draft {model} is kept on the CPU (-ngld 0 or -devd none): not counted"
        )));
    }
    Ok(Found::Draft(Draft {
        model: PathBuf::from(model),
        ctx: number(words, &["-cd", "--ctx-size-draft"])?.filter(|&n| n > 0),
        cache: (
            cache(words, &["-ctkd", "--cache-type-k-draft"])?,
            cache(words, &["-ctvd", "--cache-type-v-draft"])?,
        ),
        slots: number(words, &["-np", "--parallel"])?
            .filter(|&n| n > 0)
            .unwrap_or(1),
    }))
}

/// What the draft holds: `(weights, KV)`, from its own file. `main_ctx` is
/// the main model's total context, which an unset draft context follows.
pub fn bytes(draft: &Draft, main_ctx: u64) -> Result<(u64, u64), String> {
    let (_, weights, geometry) = crate::model_files::weights_and_geometry(Path::new(&draft.model))
        .map_err(|e| format!("draft model {}: {e}", draft.model.display()))?;
    let tokens = draft
        .ctx
        .map_or(main_ctx, |per_slot| per_slot.saturating_mul(draft.slots));
    let (k, v) = draft.cache;
    Ok((weights, geometry.kv_bytes(tokens, k, v, None)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn a_draft_and_its_settings_from_the_command_line() {
        assert_eq!(
            from_args(&words("llama-server -m big.gguf -c 8192")),
            Ok(Found::None)
        );
        assert_eq!(
            from_args(&words(
                "llama-server -m big.gguf -md small.gguf -cd 4096 -np 2 -ctkd q8_0"
            )),
            Ok(Found::Draft(Draft {
                model: PathBuf::from("small.gguf"),
                ctx: Some(4096),
                cache: (CacheType::Q8_0, CacheType::F16),
                slots: 2,
            }))
        );
        let Ok(Found::Draft(d)) = from_args(&words("llama-server --model-draft=s.gguf -cd 0"))
        else {
            panic!()
        };
        assert_eq!(
            (d.ctx, d.cache, d.slots),
            (None, (CacheType::F16, CacheType::F16), 1)
        );
        // The last occurrence wins, whatever its value.
        assert!(from_args(&words("llama-server -md s.gguf -cd 4096 -cd x")).is_err());
        for cpu in ["-ngld 0", "--gpu-layers-draft=0", "-devd none"] {
            assert!(
                matches!(
                    from_args(&words(&format!("llama-server -md s.gguf {cpu}"))),
                    Ok(Found::NotCounted(_))
                ),
                "{cpu}"
            );
        }
        assert!(matches!(
            from_args(&words("llama-server -hfd org/draft")),
            Ok(Found::NotCounted(_))
        ));
    }
}
