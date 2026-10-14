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
    for id in ids.iter().rev() {
        let at = nanos_of(id).unwrap_or(now);
        let age = ms(now.saturating_sub(at));
        let kept = tiers
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
pub const KINDS: &[&str] = &["ports", "doctor", "run"];

pub struct Journal {
    dir: PathBuf,
    keep: usize,
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
        self
    }

    pub fn at(dir: PathBuf, keep: usize) -> Result<Self, StateError> {
        ensure_dir(&dir)?;
        Ok(Self {
            dir,
            keep,
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

    /// The ids tag `name` holds, by kind.
    pub fn tag(
        &self,
        name: &str,
    ) -> Result<std::collections::BTreeMap<String, String>, StateError> {
        if !valid_tag(name) {
            return Err(StateError::BadRef(name.to_string()));
        }
        let path = self.tags_dir().join(format!("{name}.{EXTENSION}"));
        let text = std::fs::read(&path).map_err(|source| StateError::Io {
            path: path.clone(),
            source,
        })?;
        serde_json::from_slice(&text).map_err(|source| StateError::Json { path, source })
    }

    /// Every tag, by name. One tag that cannot be read fails the whole list;
    /// [`Self::tags_lenient`] skips it instead.
    pub fn tags(&self) -> Result<Tags, StateError> {
        let (tags, mut errors) = self.tags_lenient()?;
        match errors.pop() {
            Some(e) => Err(e),
            None => Ok(tags),
        }
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
        std::fs::remove_file(&path).map_err(|source| StateError::Io { path, source })
    }

    /// Drops the oldest untagged snapshots of `kind` beyond retention. A tag
    /// that cannot be read protects nothing but does not stop pruning; it is
    /// returned as the error, after pruning.
    fn prune(&self, kind: &str) -> Result<(), StateError> {
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
    fn tags_name_moments_and_survive_pruning() {
        let (j, dir) = journal("tags", 2);
        j.record("ports", &serde_json::json!({ "n": 0 })).unwrap();
        j.record("doctor", &serde_json::json!({ "d": 0 })).unwrap();
        let tagged = j.set_tag("healthy", &["ports", "doctor"]).unwrap();
        assert_eq!(tagged.len(), 2);
        for n in 1..5 {
            j.record("ports", &serde_json::json!({ "n": n })).unwrap();
        }
        assert_eq!(
            j.resolve("ports", "healthy").unwrap().body["n"],
            0,
            "kept past retention"
        );
        assert_eq!(
            j.ids("ports").unwrap().len(),
            3,
            "two kept plus the tagged one"
        );
        assert_eq!(j.resolve("doctor", "healthy").unwrap().body["d"], 0);
        assert!(j.resolve("run", "healthy").is_err());
        assert!(j.tags().unwrap().contains_key("healthy"));
        for bad in ["1abc", "~1", "../x", "A", "a/b", "a..b"] {
            assert!(j.set_tag(bad, &["ports"]).is_err(), "{bad}");
        }
        // A corrupt tag protects nothing and does not stop retention.
        std::fs::write(dir.join("tags").join("broken.json"), "{").unwrap();
        for n in 5..8 {
            let r = j.record("ports", &serde_json::json!({ "n": n })).unwrap();
            assert_eq!(r.warnings.len(), 1, "the broken tag is reported");
        }
        assert_eq!(j.ids("ports").unwrap().len(), 3, "still pruned");
        std::fs::remove_file(dir.join("tags").join("broken.json")).unwrap();
        j.delete_tag("healthy").unwrap();
        j.record("ports", &serde_json::json!({ "n": 5 })).unwrap();
        assert_eq!(
            j.ids("ports").unwrap().len(),
            2,
            "pruned again once untagged"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tiers_thin_old_snapshots_and_drop_the_oldest() {
        use crate::config::Tier;
        let minute = 60_000_000_000_u64;
        let now = 1_000 * minute;
        let id = |ago: u64| format!("{:020}-1-doctor", now - ago * minute);
        // Every minute for the last 200 minutes, oldest first.
        let ids: Vec<String> = (0..200).rev().map(id).collect();
        let tiers = [
            Tier {
                within_ms: 10 * 60_000,
                every_ms: 0,
            },
            Tier {
                within_ms: 100 * 60_000,
                every_ms: 30 * 60_000,
            },
        ];
        let (keep, drop) = thin(&ids, &tiers, now);
        assert_eq!(keep.len() + drop.len(), 200);
        // All of the last ten minutes, then about one per half hour.
        assert!(keep.contains(&id(0)) && keep.contains(&id(10)));
        let thinned = keep
            .iter()
            .filter(|k| !(0..=10).any(|a| **k == id(a)))
            .count();
        assert!((3..=4).contains(&thinned), "{thinned}");
        assert!(drop.contains(&id(150)), "past the last window");
        assert_eq!(thin(&ids, &[], now).0.len(), 200);
    }

    #[test]
    fn time_references_find_how_things_were() {
        let (j, dir) = journal("asof", 10);
        j.record("ports", &serde_json::json!({ "n": 0 })).unwrap();
        assert_eq!(j.resolve("ports", "@0s").unwrap().body["n"], 0);
        assert!(matches!(
            j.resolve("ports", "@1h"),
            Err(StateError::NotFound { .. })
        ));
        assert_eq!(
            j.ids_since("ports", Duration::from_secs(3600))
                .unwrap()
                .len(),
            1
        );
        for bad in ["@", "@5", "@5w", "@-1h", "@1.5h"] {
            assert!(
                matches!(j.resolve("ports", bad), Err(StateError::BadRef(_))),
                "{bad}"
            );
        }
        assert_eq!(parse_age("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_age("2d"), Some(Duration::from_secs(172_800)));
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
