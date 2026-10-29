//! The latest of something, kept beside the journal under a name, so a
//! lookup reads one file instead of scanning the journal for it. The
//! journal stays the record; a memo is only an index into it.

use std::hash::{Hash, Hasher};

use serde_json::Value;

use super::{EXTENSION, Journal, StateError, ensure_dir, write_synced};

/// Where memos live, beside the snapshots.
const MEMO_DIR: &str = "latest";

/// A memo's file name for `parts`: a hash, so any text (paths, server names
/// with spaces) makes a safe name.
// shortcut: std's hasher may change between Rust releases; a change only
// makes the next lookup miss until the next ready event writes it again.
#[must_use]
pub fn memo_key(kind: &str, parts: &impl Hash) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    format!("{kind}-{:016x}", h.finish())
}

impl Journal {
    /// Keeps `value` as the latest under `key` (from [`memo_key`]).
    pub fn remember(&self, key: &str, value: &Value) -> Result<(), StateError> {
        let dir = self.dir.join(MEMO_DIR);
        ensure_dir(&dir)?;
        let path = dir.join(format!("{key}.{EXTENSION}"));
        let tmp = dir.join(format!(".{key}.{}.tmp", super::unique_nanos()));
        let text = serde_json::to_vec(value).map_err(|source| StateError::Json {
            path: path.clone(),
            source,
        })?;
        write_synced(&tmp, &text)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|source| {
                let _ = std::fs::remove_file(&tmp);
                StateError::Io { path, source }
            })
    }

    /// The latest value kept under `key`, if any was and it reads.
    #[must_use]
    pub fn recall(&self, key: &str) -> Option<Value> {
        let path = self.dir.join(MEMO_DIR).join(format!("{key}.{EXTENSION}"));
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }
}
