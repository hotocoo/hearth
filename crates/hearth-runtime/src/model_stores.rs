//! Finding a GGUF by the `org/repo` name it was downloaded under, in the
//! two stores that use that naming: the Hugging Face cache (where
//! `llama-server -hf org/repo` puts it) and LM Studio's models directory.
//!
//! Names are `org/repo`, `org/repo:QUANT` (as llama.cpp's `-hf` takes them;
//! QUANT matches part of the file name, case ignored) or `org/repo/FILE`.
//! Only files are read, and nothing is downloaded.

use std::path::{Path, PathBuf};

/// `(store, repo directory for org/repo)`, for every store this machine
/// may have: the Hugging Face cache (`HF_HUB_CACHE`, else `HF_HOME/hub`,
/// else `~/.cache/huggingface/hub`), and LM Studio's models directory
/// (`~/.lmstudio/models`, and the older `~/.cache/lm-studio/models`).
fn repo_dirs(org: &str, repo: &str) -> Vec<(&'static str, PathBuf)> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = env("HOME");
    let hf_hub = env("HF_HUB_CACHE")
        .or_else(|| env("HF_HOME").map(|h| h.join("hub")))
        .or_else(|| home.as_ref().map(|h| h.join(".cache/huggingface/hub")));
    let mut out = Vec::new();
    if let Some(hub) = hf_hub {
        out.push((
            "hugging face cache",
            hub.join(format!("models--{org}--{repo}")),
        ));
    }
    if let Some(h) = &home {
        for base in [".lmstudio/models", ".cache/lm-studio/models"] {
            out.push(("lm studio", h.join(base).join(org).join(repo)));
        }
    }
    out
}

/// The directory holding a cached repo's files: for the Hugging Face cache,
/// the snapshot `refs/main` names, else the only snapshot; for LM Studio,
/// the repo directory itself.
fn files_dir(store: &str, dir: &Path) -> Option<PathBuf> {
    if store != "hugging face cache" {
        return dir.is_dir().then(|| dir.to_path_buf());
    }
    let snapshots = dir.join("snapshots");
    if let Ok(rev) = std::fs::read_to_string(dir.join("refs").join("main")) {
        let at = snapshots.join(rev.trim());
        if at.is_dir() {
            return Some(at);
        }
    }
    let mut all: Vec<PathBuf> = std::fs::read_dir(&snapshots)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    (all.len() == 1).then(|| all.remove(0))
}

/// Model weights in `dir`: `.gguf` files that are not a multimodal
/// projector, and of a split model only its first shard.
fn weights_in(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let lower = name.to_ascii_lowercase();
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
                && !lower.starts_with("mmproj")
                && (!lower.contains("-of-") || lower.contains("-00001-of-"))
        })
        .collect();
    files.sort();
    files
}

/// `org/repo[:QUANT]` or `org/repo/FILE` split into its parts, if `name`
/// has that shape.
#[must_use]
pub fn parse(name: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    let safe = |s: &str| {
        !s.is_empty()
            && s != ".."
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    };
    let (path, quant) = match name.rsplit_once(':') {
        Some((p, q)) if safe(q) => (p, Some(q.to_string())),
        _ => (name, None),
    };
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        [org, repo] if safe(org) && safe(repo) => {
            Some(((*org).into(), (*repo).into(), quant, None))
        }
        [org, repo, file] if safe(org) && safe(repo) && safe(file) && quant.is_none() => {
            Some(((*org).into(), (*repo).into(), None, Some((*file).into())))
        }
        _ => None,
    }
}

/// The weights file for `name` in the first store that has its repo.
/// `None` when no store has the repo (so another lookup may be tried);
/// an error when one does but the file is not clear.
#[must_use]
pub fn find(name: &str) -> Option<Result<PathBuf, String>> {
    let (org, repo, quant, file) = parse(name)?;
    for (store, dir) in repo_dirs(&org, &repo) {
        let Some(files) = files_dir(store, &dir) else {
            continue;
        };
        if let Some(f) = &file {
            let path = files.join(f);
            return Some(if path.is_file() {
                Ok(path)
            } else {
                Err(format!("{name}: no {f} in {}", files.display()))
            });
        }
        let candidates: Vec<PathBuf> = weights_in(&files)
            .into_iter()
            .filter(|p| {
                quant.as_ref().is_none_or(|q| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.to_ascii_lowercase().contains(&q.to_ascii_lowercase()))
                })
            })
            .collect();
        return Some(match candidates.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(format!(
                "{name}: no matching .gguf in the {store} ({})",
                files.display()
            )),
            several => Err(format!(
                "{name}: several files in the {store}; name one as {org}/{repo}/FILE or {org}/{repo}:QUANT: {}",
                several
                    .iter()
                    .filter_map(|p| p.file_name()?.to_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_split_like_llama_cpp_takes_them() {
        assert_eq!(
            parse("org/repo-GGUF:Q6_K"),
            Some(("org".into(), "repo-GGUF".into(), Some("Q6_K".into()), None))
        );
        assert_eq!(
            parse("org/repo/m.gguf").unwrap().3.as_deref(),
            Some("m.gguf")
        );
        for bad in ["qwen3:8b", "../x/y", "a/b/c/d", "a/../b"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn picks_the_weights_not_the_projector_or_later_shards() {
        let dir = std::env::temp_dir().join(format!("hearth-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in [
            "m-Q4_K.gguf",
            "m-Q8_0-00001-of-00002.gguf",
            "m-Q8_0-00002-of-00002.gguf",
            "mmproj-BF16.gguf",
            "README.md",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let names: Vec<String> = weights_in(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["m-Q4_K.gguf", "m-Q8_0-00001-of-00002.gguf"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
