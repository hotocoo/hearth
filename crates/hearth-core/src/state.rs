//! hearth's own history of what it observed on this machine.
//!
//! Every observation is one JSON document in the state directory, named so
//! that names sort by time. A snapshot is written to a hidden temporary file
//! and renamed into place, so readers never see half a document and two
//! `hearth` processes recording at once cannot interleave. Retention is a
//! count per kind from the configuration; the oldest go first.
//!
//! The daemon will move this to an embedded database. The layout here is
//! deliberately the simplest that is safe under concurrent writers.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const EXTENSION: &str = "json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub kind: String,
    pub taken_at_ms: u64,
    pub hearth_version: String,
    pub body: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("no state directory: set HEARTH_STATE, XDG_STATE_HOME or HOME to an absolute path")]
    NoLocation,
    #[error("HEARTH_STATE={0:?} must be an absolute path")]
    Relative(PathBuf),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("not a snapshot id or ~N reference: {0:?}")]
    BadRef(String),
    #[error("{id} is not a {kind} snapshot")]
    WrongKind { id: String, kind: String },
    #[error("no {kind} snapshot at {reference}: {available} recorded")]
    NotFound {
        kind: String,
        reference: String,
        available: usize,
    },
}

/// An environment variable holding a path, treating empty as unset.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Where hearth keeps its state, most specific first. Relative
/// `XDG_STATE_HOME` and `HOME` values are ignored, as the XDG specification
/// says; a relative `HEARTH_STATE` is an error, since it was asked for.
pub fn dir() -> Result<PathBuf, StateError> {
    if let Some(explicit) = env_path("HEARTH_STATE") {
        return if explicit.is_absolute() {
            Ok(explicit)
        } else {
            Err(StateError::Relative(explicit))
        };
    }
    env_path("XDG_STATE_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env_path("HOME")
                .filter(|p| p.is_absolute())
                .map(|h| h.join(".local").join("state"))
        })
        .map(|base| base.join("hearth"))
        .ok_or(StateError::NoLocation)
}

/// A snapshot that was written, and anything that went wrong tidying up
/// after it. The snapshot is on disk either way.
#[derive(Debug)]
pub struct Recorded {
    pub snapshot: Snapshot,
    pub warnings: Vec<StateError>,
}

/// Snapshot kinds are part of file names, so they are restricted to
/// lowercase letters and underscores.
fn valid_kind(kind: &str) -> bool {
    !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
}

/// `<nanoseconds since epoch>-<pid>-<kind>`: sorts by time, unique per
/// process, and safe to use as a file name.
fn valid_id(id: &str) -> bool {
    let mut parts = id.splitn(3, '-');
    let digits =
        |s: Option<&str>| s.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    digits(parts.next()) && digits(parts.next()) && parts.next().is_some_and(valid_kind)
}

fn kind_of(id: &str) -> Option<&str> {
    id.splitn(3, '-').nth(2)
}

pub struct Journal {
    dir: PathBuf,
    keep: usize,
}

impl Journal {
    /// The journal in [`dir`], keeping `keep` snapshots of each kind.
    pub fn open(keep: usize) -> Result<Self, StateError> {
        Self::at(dir()?, keep)
    }

    /// Snapshots hold process lists, executable paths and model paths, so the
    /// directory is created readable by its owner only.
    pub fn at(dir: PathBuf, keep: usize) -> Result<Self, StateError> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir).map_err(|source| StateError::Io {
            path: dir.clone(),
            source,
        })?;
        Ok(Self { dir, keep })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Records `body` as a new snapshot of `kind` and drops the oldest
    /// snapshots of that kind beyond the retention count.
    pub fn record<T: Serialize>(&self, kind: &str, body: &T) -> Result<Recorded, StateError> {
        if !valid_kind(kind) {
            return Err(StateError::BadRef(kind.to_string()));
        }
        let nanos = unique_nanos();
        let id = format!("{nanos:020}-{}-{kind}", std::process::id());
        let snapshot = Snapshot {
            taken_at_ms: u64::try_from(std::time::Duration::from_nanos(nanos).as_millis())
                .unwrap_or(u64::MAX),
            hearth_version: env!("CARGO_PKG_VERSION").to_string(),
            kind: kind.to_string(),
            body: serde_json::to_value(body).map_err(|source| StateError::Json {
                path: self.dir.clone(),
                source,
            })?,
            id,
        };
        let path = self.file(&snapshot.id);
        let tmp = self.dir.join(format!(".{}.tmp", snapshot.id));
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StateError::Io { path, source }
        };
        let text = serde_json::to_vec(&snapshot).map_err(|source| StateError::Json {
            path: path.clone(),
            source,
        })?;
        let written = write_synced(&tmp, &text)
            .map_err(io(&tmp))
            .and_then(|()| std::fs::rename(&tmp, &path).map_err(io(&path)));
        if let Err(e) = written {
            // Nothing else would ever remove it: readers skip temporaries.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(Recorded {
            snapshot,
            warnings: self.prune(kind).err().into_iter().collect(),
        })
    }

    fn file(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{EXTENSION}"))
    }

    /// Snapshot ids of `kind`, oldest first.
    pub fn ids(&self, kind: &str) -> Result<Vec<String>, StateError> {
        let entries = std::fs::read_dir(&self.dir).map_err(|source| StateError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let suffix = format!(".{EXTENSION}");
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StateError::Io {
                path: self.dir.clone(),
                source,
            })?;
            // Names that are not UTF-8 are not hearth's.
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(id) = name.strip_suffix(&suffix)
                && valid_id(id)
                && kind_of(id) == Some(kind)
            {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn read(&self, id: &str) -> Result<Snapshot, StateError> {
        if !valid_id(id) {
            return Err(StateError::BadRef(id.to_string()));
        }
        let path = self.file(id);
        let text = std::fs::read(&path).map_err(|source| StateError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_slice(&text).map_err(|source| StateError::Json { path, source })
    }

    /// Resolves a full id, or `~N` meaning N snapshots of `kind` before the
    /// latest (`~0` is the latest).
    pub fn resolve(&self, kind: &str, reference: &str) -> Result<Snapshot, StateError> {
        let Some(back) = reference.strip_prefix('~') else {
            if kind_of(reference) != Some(kind) {
                return Err(StateError::WrongKind {
                    id: reference.to_string(),
                    kind: kind.to_string(),
                });
            }
            return self.read(reference);
        };
        let back: usize = back
            .parse()
            .map_err(|_| StateError::BadRef(reference.to_string()))?;
        let ids = self.ids(kind)?;
        let not_found = || StateError::NotFound {
            kind: kind.to_string(),
            reference: reference.to_string(),
            available: ids.len(),
        };
        let index = back
            .checked_add(1)
            .and_then(|n| ids.len().checked_sub(n))
            .ok_or_else(not_found)?;
        self.read(&ids[index])
    }

    /// The most recent `n` snapshots of `kind`, oldest first. Unreadable
    /// files are returned as errors so callers can report them; files that
    /// a concurrent prune removed after they were listed are skipped.
    pub fn recent(
        &self,
        kind: &str,
        n: usize,
    ) -> Result<Vec<Result<Snapshot, StateError>>, StateError> {
        let ids = self.ids(kind)?;
        let start = ids.len().saturating_sub(n);
        Ok(ids[start..]
            .iter()
            .map(|id| self.read(id))
            .filter(|r| {
                !matches!(r, Err(StateError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound)
            })
            .collect())
    }

    fn prune(&self, kind: &str) -> Result<(), StateError> {
        let ids = self.ids(kind)?;
        let excess = ids.len().saturating_sub(self.keep);
        for id in &ids[..excess] {
            let path = self.file(id);
            match std::fs::remove_file(&path) {
                // Another process pruning at the same time got there first.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(StateError::Io { path, source }),
                Ok(()) => {}
            }
        }
        Ok(())
    }
}

/// Nanoseconds since the epoch, strictly increasing within this process:
/// the system clock's resolution is coarser than nanoseconds on some
/// platforms (microseconds on macOS), and two snapshots must never share an
/// id.
fn unique_nanos() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn journal(test: &str, keep: usize) -> (Journal, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hearth-state-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Journal::at(dir.clone(), keep).unwrap(), dir)
    }

    #[test]
    fn records_resolves_and_prunes() {
        let (j, dir) = journal("prunes", 2);
        for n in 0..3 {
            j.record("ports", &serde_json::json!({ "n": n })).unwrap();
        }
        j.record("doctor", &serde_json::json!({})).unwrap();
        assert_eq!(j.ids("ports").unwrap().len(), 2, "oldest pruned");
        assert_eq!(j.ids("doctor").unwrap().len(), 1, "kinds pruned separately");
        assert_eq!(j.resolve("ports", "~0").unwrap().body["n"], 2);
        // Records made back to back keep distinct ids.
        assert_eq!(j.resolve("ports", "~1").unwrap().body["n"], 1);
        for beyond in ["~2", &format!("~{}", usize::MAX)] {
            assert!(matches!(
                j.resolve("ports", beyond),
                Err(StateError::NotFound { available: 2, .. })
            ));
        }
        let id = j.ids("ports").unwrap()[0].clone();
        assert_eq!(j.resolve("ports", &id).unwrap().id, id);
        assert!(matches!(
            j.resolve("doctor", &id),
            Err(StateError::WrongKind { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_references_that_escape_the_directory() {
        let (j, dir) = journal("escape", 1);
        for bad in [
            "../x",
            "1-2-../../etc/passwd",
            "1-2-Ports",
            "~x",
            "",
            "1--ports",
        ] {
            assert!(j.resolve("ports", bad).is_err(), "{bad}");
        }
        assert!(j.record("../x", &()).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ignores_temporaries_and_foreign_files() {
        let (j, dir) = journal("foreign", 5);
        j.record("ports", &()).unwrap();
        std::fs::write(dir.join(".1-1-ports.tmp"), "{").unwrap();
        std::fs::write(dir.join("notes.json"), "{}").unwrap();
        assert_eq!(j.ids("ports").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
