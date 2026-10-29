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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    #[error("not a snapshot id, ~N reference, @AGE (90s, 30m, 2h, 1d) or tag: {0:?}")]
    BadRef(String),
    #[error("no tag named {0:?}; `hearth tag` lists them")]
    NoTag(String),
    #[error(
        "already {0} tags, the most state.max_tags allows: delete one (`hearth tag --delete NAME`) or move an existing name"
    )]
    TooManyTags(usize),
    #[error("tag {tag:?} has no {kind} snapshot")]
    TagWithout { tag: String, kind: String },
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

/// Creates `dir` and its parents when missing. Snapshots hold process lists,
/// executable paths and model paths, so what is created is readable by its
/// owner only; an existing directory is left as it is.
pub fn ensure_dir(dir: &Path) -> Result<(), StateError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir).map_err(|source| StateError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

/// Tags by name, each with the snapshot id it names per kind.
pub type Tags = std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>;

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

/// Splits `ids` (oldest first) into those `tiers` keep and those they drop,
/// at time `now` (ns). Within a tier's window, the newest snapshot of each
/// `every_ms` interval is kept; past the last window nothing is. No tiers
/// keeps everything.
fn thin(ids: &[String], tiers: &[crate::config::Tier], now: u64) -> (Vec<String>, Vec<String>) {
    if tiers.is_empty() {
        return (ids.to_vec(), Vec::new());
    }
    let ms = |ns: u64| ns / u64::try_from(Duration::from_millis(1).as_nanos()).unwrap_or(1);
    let mut seen = std::collections::BTreeSet::new();
    let (mut keep, mut drop) = (Vec::new(), Vec::new());
    for (newest, id) in ids.iter().rev().enumerate().map(|(n, id)| (n == 0, id)) {
        let at = nanos_of(id).unwrap_or(now);
        let age = ms(now.saturating_sub(at));
        // The newest is what was just recorded: whatever the tiers say, it
        // is kept, or recording would save nothing.
        let kept = newest
            || tiers
                .iter()
                .enumerate()
                .find(|(_, t)| age <= t.within_ms)
                .is_some_and(|(i, t)| t.every_ms == 0 || seen.insert((i, ms(at) / t.every_ms)));
        if kept {
            keep.push(id.clone());
        } else {
            drop.push(id.clone());
        }
    }
    keep.reverse();
    (keep, drop)
}

/// `90s`, `30m`, `2h`, `1d`: a length of time back from now. Units are the
/// clock's, not policy.
#[must_use]
pub fn parse_age(text: &str) -> Option<Duration> {
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = text.split_at(split);
    let n: u64 = n.parse().ok()?;
    let unit = match unit {
        "s" => Duration::from_secs(1),
        "m" => Duration::from_secs(60),
        "h" => Duration::from_secs(60 * 60),
        "d" => Duration::from_secs(24 * 60 * 60),
        _ => return None,
    };
    unit.checked_mul(u32::try_from(n).ok()?)
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// When snapshot `id` was taken, from its name.
fn nanos_of(id: &str) -> Option<u64> {
    id.split('-').next()?.parse().ok()
}

/// A tag file that is not there is a tag that does not exist.
fn tag_error(name: &str, path: PathBuf, source: std::io::Error) -> StateError {
    if source.kind() == std::io::ErrorKind::NotFound {
        StateError::NoTag(name.to_string())
    } else {
        StateError::Io { path, source }
    }
}

/// Tag names start with a letter, so they can never be mistaken for an id
/// (digits first) or a `~N` reference, and are safe as file names.
fn valid_tag(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
        && !name.contains("..")
}

fn kind_of(id: &str) -> Option<&str> {
    id.splitn(3, '-').nth(2)
}

/// The kinds hearth records: what `state.thinned_kinds` may name.
pub const KINDS: &[&str] = &["ports", "doctor", "run", "note"];

/// The kinds that are states of the machine, which may be thinned with
/// age. The others are events (run events, notes): every one matters.
pub const STATE_KINDS: &[&str] = &["ports", "doctor"];

pub struct Journal {
    dir: PathBuf,
    keep: usize,
    /// Most tags; tagged snapshots are never pruned, so this bounds them.
    max_tags: usize,
    thinned: Vec<String>,
    tiers: Vec<crate::config::Tier>,
}

impl Journal {
    /// The journal in [`dir`], keeping `keep` snapshots of each kind.
    pub fn open(state: &crate::config::State) -> Result<Self, StateError> {
        Ok(Self::at(dir()?, state.max_snapshots_per_kind)?.thinning(state))
    }

    /// Thins the kinds `state` names by its age tiers.
    #[must_use]
    pub fn thinning(mut self, state: &crate::config::State) -> Self {
        self.thinned.clone_from(&state.thinned_kinds);
        self.tiers.clone_from(&state.tiers);
        self.max_tags = state.max_tags;
        self
    }

    pub fn at(dir: PathBuf, keep: usize) -> Result<Self, StateError> {
        ensure_dir(&dir)?;
        Ok(Self {
            dir,
            keep,
            max_tags: usize::MAX,
            thinned: Vec::new(),
            tiers: Vec::new(),
        })
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
        // After the newest of its kind even if the clock stepped back (NTP,
        // a resumed VM), so the latest snapshot is always ~0.
        let after = self
            .ids(kind)
            .ok()
            .and_then(|ids| ids.last().and_then(|id| nanos_of(id)))
            .map_or(0, |n| n.saturating_add(1));
        let nanos = unique_nanos().max(after);
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
        if let Some(ago) = reference.strip_prefix('@') {
            let age = parse_age(ago).ok_or_else(|| StateError::BadRef(reference.to_string()))?;
            return self.as_of(kind, age, reference);
        }
        if valid_tag(reference) {
            let ids = self.tag(reference)?;
            let id = ids.get(kind).ok_or_else(|| StateError::TagWithout {
                tag: reference.to_string(),
                kind: kind.to_string(),
            })?;
            return self.read(id);
        }
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

    /// The latest snapshot of `kind` taken at least `age` ago: how things
    /// were then.
    fn as_of(&self, kind: &str, age: Duration, reference: &str) -> Result<Snapshot, StateError> {
        let cutoff = now_nanos().saturating_sub(u64::try_from(age.as_nanos()).unwrap_or(u64::MAX));
        let ids = self.ids(kind)?;
        ids.iter()
            .rev()
            .find(|id| nanos_of(id).is_some_and(|n| n <= cutoff))
            .map_or_else(
                || {
                    Err(StateError::NotFound {
                        kind: kind.to_string(),
                        reference: reference.to_string(),
                        available: ids.len(),
                    })
                },
                |id| self.read(id),
            )
    }

    /// Ids of `kind` recorded within `age` of now, oldest first.
    pub fn ids_since(&self, kind: &str, age: Duration) -> Result<Vec<String>, StateError> {
        let cutoff = now_nanos().saturating_sub(u64::try_from(age.as_nanos()).unwrap_or(u64::MAX));
        Ok(self
            .ids(kind)?
            .into_iter()
            .filter(|id| nanos_of(id).is_some_and(|n| n >= cutoff))
            .collect())
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

    /// How much history of `kind` there is: snapshots, oldest and newest time
    /// (ms since the epoch), and bytes on disk.
    pub fn stats(&self, kind: &str) -> Result<(usize, Option<u64>, Option<u64>, u64), StateError> {
        let ids = self.ids(kind)?;
        let ms = |id: &String| {
            nanos_of(id)
                .map(|n| u64::try_from(Duration::from_nanos(n).as_millis()).unwrap_or(u64::MAX))
        };
        let bytes = ids
            .iter()
            .filter_map(|id| std::fs::metadata(self.file(id)).ok())
            .map(|m| m.len())
            .sum();
        Ok((
            ids.len(),
            ids.first().and_then(ms),
            ids.last().and_then(ms),
            bytes,
        ))
    }

    fn remove(&self, id: &str) -> Result<(), StateError> {
        let path = self.file(id);
        match std::fs::remove_file(&path) {
            // Another process pruning at the same time got there first.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StateError::Io { path, source }),
            Ok(()) => Ok(()),
        }
    }

    fn tags_dir(&self) -> PathBuf {
        self.dir.join("tags")
    }

    /// Refuses a new tag name once `max_tags` exist; moving one is fine.
    fn room_for_tag(&self, path: &Path) -> Result<(), StateError> {
        if path.exists() {
            return Ok(());
        }
        let count = std::fs::read_dir(self.tags_dir()).map_or(0, |entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .ends_with(&format!(".{EXTENSION}"))
                })
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .count()
        });
        if count >= self.max_tags {
            return Err(StateError::TooManyTags(self.max_tags));
        }
        Ok(())
    }

    /// Holds the journal's lock until dropped, so a prune and a tag in two
    /// processes cannot interleave (a prune removing a snapshot a tag just
    /// named). `None` where the filesystem has no locks: the checks after
    /// each write still catch most races.
    fn lock(&self) -> Option<std::fs::File> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(".lock"))
            .ok()?;
        file.lock().ok()?;
        Some(file)
    }

    /// Names the latest snapshot of every kind `name`, so it can be referred
    /// to by that name and is never pruned. Naming a moment again moves the
    /// name. Returns the ids tagged, by kind.
    pub fn set_tag(
        &self,
        name: &str,
        kinds: &[&str],
    ) -> Result<std::collections::BTreeMap<String, String>, StateError> {
        if !valid_tag(name) {
            return Err(StateError::BadRef(name.to_string()));
        }
        let latest = || -> Result<std::collections::BTreeMap<String, String>, StateError> {
            let mut ids = std::collections::BTreeMap::new();
            for kind in kinds {
                if let Some(id) = self.ids(kind)?.pop() {
                    ids.insert((*kind).to_string(), id);
                }
            }
            Ok(ids)
        };
        let dir = self.tags_dir();
        ensure_dir(&dir)?;
        let path = dir.join(format!("{name}.{EXTENSION}"));
        let _held = self.lock();
        self.room_for_tag(&path)?;
        // A prune that ran between choosing the ids and writing the tag may
        // have removed one; choose again until the tag names what exists.
        loop {
            let ids = latest()?;
            let text = serde_json::to_vec(&ids).map_err(|source| StateError::Json {
                path: path.clone(),
                source,
            })?;
            let tmp = dir.join(format!(".{name}.{}.tmp", unique_nanos()));
            let written = write_synced(&tmp, &text).and_then(|()| std::fs::rename(&tmp, &path));
            if let Err(source) = written {
                let _ = std::fs::remove_file(&tmp);
                return Err(StateError::Io { path, source });
            }
            if ids.values().all(|id| self.file(id).exists()) {
                return Ok(ids);
            }
        }
    }

    /// Names one recorded snapshot `name`, like `git tag NAME COMMIT`:
    /// the tag then holds only that snapshot. Naming it again moves it.
    pub fn tag_snapshot(&self, name: &str, id: &str) -> Result<(), StateError> {
        if !valid_tag(name) {
            return Err(StateError::BadRef(name.to_string()));
        }
        let kind = kind_of(id)
            .filter(|_| valid_id(id))
            .ok_or_else(|| StateError::BadRef(id.to_string()))?;
        let ids = std::collections::BTreeMap::from([(kind.to_string(), id.to_string())]);
        let file = self.file(id);
        let gone = || StateError::Io {
            path: file.clone(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };
        let _held = self.lock();
        if !file.exists() {
            return Err(gone());
        }
        let dir = self.tags_dir();
        ensure_dir(&dir)?;
        let path = dir.join(format!("{name}.{EXTENSION}"));
        self.room_for_tag(&path)?;
        // What the name held before, to put back if the snapshot is pruned
        // before the new tag protects it.
        let before = match std::fs::read(&path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(StateError::Io { path, source }),
        };
        let text = serde_json::to_vec(&ids).map_err(|source| StateError::Json {
            path: path.clone(),
            source,
        })?;
        let tmp = dir.join(format!(".{name}.{}.tmp", unique_nanos()));
        let written = write_synced(&tmp, &text).and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(source) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(StateError::Io { path, source });
        }
        // A prune may have removed it before the tag protected it.
        if file.exists() {
            return Ok(());
        }
        match before {
            Some(b) => {
                let tmp = dir.join(format!(".{name}.{}.tmp", unique_nanos()));
                write_synced(&tmp, &b)
                    .and_then(|()| std::fs::rename(&tmp, &path))
                    .map_err(|source| StateError::Io {
                        path: path.clone(),
                        source,
                    })?;
            }
            None => self.delete_tag(name)?,
        }
        Err(gone())
    }

    /// The ids tag `name` holds, by kind.
    pub fn tag(
        &self,
        name: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, StateError> {
        if !valid_tag(name) {
            return Err(StateError::BadRef(name.to_string()));
        }
        let path = self.tags_dir().join(format!("{name}.{EXTENSION}"));
        let text = std::fs::read(&path).map_err(|source| tag_error(name, path.clone(), source))?;
        serde_json::from_slice(&text).map_err(|source| StateError::Json { path, source })
    }

    /// Every readable tag, and the errors for those that are not.
    pub fn tags_lenient(&self) -> Result<(Tags, Vec<StateError>), StateError> {
        let dir = self.tags_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Tags::default(), Vec::new()));
            }
            Err(source) => return Err(StateError::Io { path: dir, source }),
        };
        let mut out = Tags::new();
        let mut errors = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StateError::Io {
                path: dir.clone(),
                source,
            })?;
            let Ok(file) = entry.file_name().into_string() else {
                continue;
            };
            if let Some(name) = file.strip_suffix(&format!(".{EXTENSION}"))
                && valid_tag(name)
            {
                match self.tag(name) {
                    Ok(ids) => {
                        out.insert(name.to_string(), ids);
                    }
                    // Deleted between listing and reading: not a tag any more.
                    Err(StateError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => errors.push(e),
                }
            }
        }
        Ok((out, errors))
    }

    /// Removes tag `name`; its snapshots are pruned like any other again.
    pub fn delete_tag(&self, name: &str) -> Result<(), StateError> {
        if !valid_tag(name) {
            return Err(StateError::BadRef(name.to_string()));
        }
        let path = self.tags_dir().join(format!("{name}.{EXTENSION}"));
        std::fs::remove_file(&path).map_err(|source| tag_error(name, path, source))
    }

    /// Drops the oldest untagged snapshots of `kind` beyond retention. A tag
    /// that cannot be read protects nothing but does not stop pruning; it is
    /// returned as the error, after pruning.
    fn prune(&self, kind: &str) -> Result<(), StateError> {
        let _held = self.lock();
        let (tags, mut bad) = self.tags_lenient()?;
        let tagged: std::collections::BTreeSet<String> = tags
            .into_values()
            .flat_map(std::collections::BTreeMap::into_values)
            .collect();
        // Tagged snapshots are kept and do not count against retention.
        let mut ids: Vec<String> = self
            .ids(kind)?
            .into_iter()
            .filter(|id| !tagged.contains(id))
            .collect();
        if self.thinned.iter().any(|k| k == kind) {
            let (keep, drop) = thin(&ids, &self.tiers, now_nanos());
            for id in &drop {
                self.remove(id)?;
            }
            ids = keep;
        }
        let excess = ids.len().saturating_sub(self.keep);
        for id in &ids[..excess] {
            self.remove(id)?;
        }
        match bad.pop() {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
mod tests;
