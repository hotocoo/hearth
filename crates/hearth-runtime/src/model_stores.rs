//! Finding a GGUF by the `org/repo` name it was downloaded under, in the
//! two stores that use that naming: the Hugging Face cache (where
//! `llama-server -hf org/repo` puts it) and LM Studio's models directory.
//!
//! Names are `org/repo`, `org/repo:QUANT` (as llama.cpp's `-hf` takes them;
//! QUANT matches part of the file name, case ignored) or `org/repo/FILE`.
//! Only files are read, and nothing is downloaded.

use std::path::{Path, PathBuf};

/// The download stores searched, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    HuggingFace,
    LmStudio,
}

impl Store {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::HuggingFace => "hugging face cache",
            Self::LmStudio => "lm studio models directory",
        }
    }
}

/// The download stores this machine may have, each with its base
/// directory, in the locations those tools themselves use: the Hugging
/// Face cache (`HF_HUB_CACHE`, `HUGGINGFACE_HUB_CACHE`, `HF_HOME/hub`, else
/// `XDG_CACHE_HOME` or `~/.cache`, then `huggingface/hub`), and LM Studio's
/// models directory (`~/.lmstudio/models`, and the older
/// `~/.cache/lm-studio/models`).
fn bases() -> Vec<(Store, PathBuf)> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = env("HOME");
    let hf_hub = env("HF_HUB_CACHE")
        .or_else(|| env("HUGGINGFACE_HUB_CACHE"))
        .or_else(|| env("HF_HOME").map(|h| h.join("hub")))
        .or_else(|| {
            env("XDG_CACHE_HOME")
                .or_else(|| home.as_ref().map(|h| h.join(".cache")))
                .map(|c| c.join("huggingface").join("hub"))
        });
    let mut out = Vec::new();
    if let Some(hub) = hf_hub {
        out.push((Store::HuggingFace, hub));
    }
    if let Some(h) = &home {
        for base in [".lmstudio/models", ".cache/lm-studio/models"] {
            out.push((Store::LmStudio, h.join(base)));
        }
    }
    out
}

/// `(store, repo directory for org/repo)` in every store.
fn repo_dirs(org: &str, repo: &str) -> Vec<(Store, PathBuf)> {
    bases()
        .into_iter()
        .map(|(store, base)| {
            let dir = match store {
                Store::HuggingFace => base.join(format!("models--{org}--{repo}")),
                Store::LmStudio => base.join(org).join(repo),
            };
            (store, dir)
        })
        .collect()
}

/// Every `org/repo` each store has, sorted, for listing what is on this
/// machine, and the directories that could not be read. A store that is
/// absent lists nothing.
#[must_use]
pub fn list() -> (Vec<(Store, String)>, Vec<String>) {
    let mut errors = Vec::new();
    let mut dirs = |d: &Path| -> Vec<String> {
        match crate::ollama_store::subdirs(d) {
            Ok(names) => names,
            Err(e) => {
                errors.push(e);
                Vec::new()
            }
        }
    };
    let mut out = Vec::new();
    for (store, base) in bases() {
        match store {
            Store::HuggingFace => {
                for name in dirs(&base) {
                    if let Some((org, repo)) = name
                        .strip_prefix("models--")
                        .and_then(|r| r.split_once("--"))
                    {
                        out.push((store, format!("{org}/{repo}")));
                    }
                }
            }
            Store::LmStudio => {
                for org in dirs(&base) {
                    for repo in dirs(&base.join(&org)) {
                        out.push((store, format!("{org}/{repo}")));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out.dedup_by(|a, b| a == b);
    (out, errors)
}

/// The weight files of repo `org/repo` in `store`, each as a name
/// [`find_in`] resolves (`org/repo/FILE`), or the repo itself when it is a
/// safetensors model. Empty when the store has none.
pub fn weights_of(name: &str, store: Store) -> Result<Vec<String>, String> {
    let Some((org, repo, None, None)) = parse(name) else {
        return Err(format!(
            "{name}: has characters hearth cannot look up by name (only letters, digits, '-', '_', '.')"
        ));
    };
    let Some((_, dir)) = repo_dirs(&org, &repo)
        .into_iter()
        .find(|(s, d)| *s == store && d.is_dir())
    else {
        return Ok(Vec::new());
    };
    let Some(files) = files_dir(store, &dir)? else {
        return Ok(Vec::new());
    };
    let ggufs = weights_in(&files)?;
    if ggufs.is_empty() {
        return Ok(match find_in(name, store) {
            Some(Ok(p)) if p.is_dir() => vec![name.to_string()],
            _ => Vec::new(),
        });
    }
    Ok(ggufs
        .iter()
        .filter_map(|p| relative(p, &files).map(|f| format!("{org}/{repo}/{f}")))
        .collect())
}

/// The directory holding a cached repo's files: for the Hugging Face cache,
/// the snapshot `refs/main` names, else the only snapshot; for LM Studio,
/// the repo directory itself. `Ok(None)` when the store does not have the
/// repo; an error when it does but which snapshot to use is unclear.
fn files_dir(store: Store, dir: &Path) -> Result<Option<PathBuf>, String> {
    if store == Store::LmStudio {
        return Ok(dir.is_dir().then(|| dir.to_path_buf()));
    }
    if !dir.is_dir() {
        return Ok(None);
    }
    let snapshots = dir.join("snapshots");
    if let Ok(rev) = std::fs::read_to_string(dir.join("refs").join("main")) {
        let rev = rev.trim();
        // A revision is a commit hash, never a path.
        let valid = !rev.is_empty() && rev.bytes().all(|b| b.is_ascii_alphanumeric());
        if valid && snapshots.join(rev).is_dir() {
            return Ok(Some(snapshots.join(rev)));
        }
    }
    let entries = match std::fs::read_dir(&snapshots) {
        // A download that never finished has no snapshot yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        r => r.map_err(|e| format!("{}: {e}", snapshots.display()))?,
    };
    let mut all: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    match all.len() {
        1 => Ok(Some(all.remove(0))),
        0 => Ok(None),
        n => Err(format!(
            "{} has {n} snapshots and no usable refs/main; name the file with its snapshot path",
            dir.display()
        )),
    }
}

/// Whether `name` is a later shard of a split model: `-NNNNN-of-NNNNN.gguf`
/// with an index above 1, the naming llama.cpp's split tool uses.
fn later_shard(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".gguf") else {
        return false;
    };
    let Some((head, total)) = stem.rsplit_once("-of-") else {
        return false;
    };
    let Some((_, index)) = head.rsplit_once('-') else {
        return false;
    };
    let digits = |s: &str| s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit());
    digits(index) && digits(total) && index != "00001"
}

/// Model weights in `dir`: `.gguf` files that are not a multimodal
/// projector, and of a split model only its first shard.
fn weights_in(dir: &Path) -> Result<Vec<PathBuf>, String> {
    // Repos often keep each quantization in its own folder
    // (`Q4_K_M/m-Q4_K_M-00001-of-00002.gguf`), so folders are walked too.
    // Symlinked folders are not followed, so a link loop cannot recurse;
    // symlinked files (the Hugging Face cache's) are read as usual.
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    let failed = |d: &Path, e: std::io::Error| format!("{}: {e}", d.display());
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| failed(&d, e))? {
            let entry = entry.map_err(|e| failed(&d, e))?;
            let kind = entry.file_type().map_err(|e| failed(&d, e))?;
            let p = entry.path();
            if kind.is_dir() {
                pending.push(p);
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let lower = name.to_ascii_lowercase();
            if p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("gguf"))
                && !lower.contains("mmproj")
                && !later_shard(&lower)
            {
                files.push(p);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// `path` relative to `base`, with `/` between its parts, as a name's
/// `FILE` part.
fn relative(path: &Path, base: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = path
        .strip_prefix(base)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect();
    Some(parts?.join("/"))
}

/// `org/repo[:QUANT]` or `org/repo/FILE` split into its parts, if `name`
/// has that shape.
#[must_use]
pub fn parse(name: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    let safe = |s: &str| {
        !s.is_empty()
            && s != ".."
            && s != "."
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
        // FILE may be in a folder of the repo: org/repo/Q4_K_M/file.gguf.
        [org, repo, file @ ..]
            if safe(org)
                && safe(repo)
                && !file.is_empty()
                && file.iter().all(|f| safe(f))
                && quant.is_none() =>
        {
            Some(((*org).into(), (*repo).into(), None, Some(file.join("/"))))
        }
        _ => None,
    }
}

/// The weights file for `name` in the first store that has its repo.
/// `None` when no store has the repo (so another lookup may be tried);
/// an error when one does but the file is not clear.
#[must_use]
pub fn find(name: &str) -> Option<Result<PathBuf, String>> {
    search(name, None)
}

/// [`find`] in one store only.
#[must_use]
pub fn find_in(name: &str, store: Store) -> Option<Result<PathBuf, String>> {
    search(name, Some(store))
}

fn search(name: &str, only: Option<Store>) -> Option<Result<PathBuf, String>> {
    let (org, repo, quant, file) = parse(name)?;
    for (store, dir) in repo_dirs(&org, &repo) {
        if only.is_some_and(|o| o != store) {
            continue;
        }
        let files = match files_dir(store, &dir) {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => return Some(Err(e)),
        };
        if let Some(f) = &file {
            let path = files.join(f);
            return Some(if path.is_file() {
                Ok(path)
            } else {
                Err(format!("{name}: no {f} in {}", files.display()))
            });
        }
        let found = match weights_in(&files) {
            Ok(f) => f,
            Err(e) => return Some(Err(e)),
        };
        let candidates: Vec<PathBuf> = found
            .into_iter()
            .filter(|p| {
                // The quantization may name the file or its folder.
                quant.as_ref().is_none_or(|q| {
                    relative(p, &files)
                        .is_some_and(|n| n.to_ascii_lowercase().contains(&q.to_ascii_lowercase()))
                })
            })
            .collect();
        let store = store.name();
        // A repo of safetensors with a config.json (MLX, Transformers) is
        // fitted as a whole directory.
        if candidates.is_empty() && quant.is_none() && files.join("config.json").is_file() {
            let entries =
                match std::fs::read_dir(&files).and_then(Iterator::collect::<Result<Vec<_>, _>>) {
                    Ok(e) => e,
                    Err(e) => return Some(Err(format!("{}: {e}", files.display()))),
                };
            if entries.iter().any(|e| {
                e.path()
                    .extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("safetensors"))
            }) {
                return Some(Ok(files));
            }
        }
        return Some(match candidates.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(format!(
                "{name}: no matching .gguf (nor .safetensors with config.json) in the {store} ({})",
                files.display()
            )),
            several => Err(format!(
                "{name}: several files in the {store}; name one as {org}/{repo}/FILE or {org}/{repo}:QUANT (hearth models {org}/{repo} sizes each): {}",
                several
                    .iter()
                    .filter_map(|p| relative(p, &files))
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
        assert_eq!(
            parse("org/repo/Q4_K_M/m.gguf").unwrap().3.as_deref(),
            Some("Q4_K_M/m.gguf")
        );
        for bad in [
            "qwen3:8b", "../x/y", "a/b/../d", "a/../b", "a/b/", "a/b/c:Q4",
        ] {
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
            "m-mmproj-F16.gguf",
            "Lord-of-the-Rings-7B.gguf",
            "README.md",
        ] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        std::fs::create_dir_all(dir.join("Q4_K_M")).unwrap();
        std::fs::write(dir.join("Q4_K_M/m-Q4_K_M-00001-of-00002.gguf"), "").unwrap();
        std::fs::write(dir.join("Q4_K_M/m-Q4_K_M-00002-of-00002.gguf"), "").unwrap();
        let names: Vec<String> = weights_in(&dir)
            .unwrap()
            .iter()
            .map(|p| relative(p, &dir).unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "Lord-of-the-Rings-7B.gguf",
                "Q4_K_M/m-Q4_K_M-00001-of-00002.gguf",
                "m-Q4_K.gguf",
                "m-Q8_0-00001-of-00002.gguf"
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
