//! Layered configuration.
//!
//! The base layer is `config/hearth.toml`, embedded at build time, so that no
//! value lives in Rust code. A user file, when present, is merged over it key
//! by key. Unknown keys are rejected so a typo cannot silently fall back to the
//! base value.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The shipped configuration, the single place non-measured values live.
pub const BASE: &str = include_str!("../config/hearth.toml");

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub discovery: Discovery,
    pub state: State,
    pub supervise: Supervise,
    pub notify: Notify,
    pub watch: Watch,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watch {
    pub doctor_interval_ms: u64,
    pub ports_interval_ms: u64,
    pub limit_horizon_ms: u64,
    pub confirm_rounds: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notify {
    pub desktop: bool,
    pub webhook_url: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub first_contact_timeout_ms: u64,
    pub max_probe_response_bytes: u64,
    pub learned_deadline_multiplier: f64,
    pub learned_min_samples: usize,
    pub learned_deadline_floor_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supervise {
    pub restart_budget: u32,
    pub restart_window_ms: u64,
    pub backoff_start_ms: u64,
    pub backoff_max_ms: u64,
    pub backoff_factor: u32,
    pub ready_timeout_ms: u64,
    pub stop_grace_ms: u64,
    pub probe_interval_ms: u64,
    pub unresponsive_probes: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub max_snapshots_per_kind: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{origin}: {source}")]
    Parse {
        origin: String,
        source: toml::de::Error,
    },
    #[error("{key} = {value}: {reason}")]
    Invalid {
        key: &'static str,
        value: String,
        reason: &'static str,
    },
}

const AT_LEAST_ONE: &str = "must be at least 1";

fn invalid(
    key: &'static str,
    value: &dyn std::fmt::Display,
    reason: &'static str,
) -> Result<(), ConfigError> {
    Err(ConfigError::Invalid {
        key,
        value: value.to_string(),
        reason,
    })
}

impl Config {
    /// Rejects values that parse but cannot mean anything.
    fn validate(self) -> Result<Self, ConfigError> {
        self.positive()?;
        self.relations()?;
        self.representable()?;
        if let Some(url) = &self.notify.webhook_url
            && !(url.starts_with("http://") || url.starts_with("https://"))
        {
            invalid(
                "notify.webhook_url",
                url,
                "must be an http:// or https:// URL",
            )?;
        }
        Ok(self)
    }

    /// Counts and durations for which zero means nothing.
    fn positive(&self) -> Result<(), ConfigError> {
        let (d, v, w) = (&self.discovery, &self.supervise, &self.watch);
        let gone = "must be at least 1, or a snapshot would be deleted as it is written";
        for (key, value, reason) in [
            (
                "discovery.first_contact_timeout_ms",
                d.first_contact_timeout_ms,
                AT_LEAST_ONE,
            ),
            (
                "discovery.learned_min_samples",
                d.learned_min_samples as u64,
                AT_LEAST_ONE,
            ),
            (
                "state.max_snapshots_per_kind",
                self.state.max_snapshots_per_kind as u64,
                gone,
            ),
            ("notify.timeout_ms", self.notify.timeout_ms, AT_LEAST_ONE),
            (
                "watch.doctor_interval_ms",
                w.doctor_interval_ms,
                AT_LEAST_ONE,
            ),
            ("watch.ports_interval_ms", w.ports_interval_ms, AT_LEAST_ONE),
            ("watch.limit_horizon_ms", w.limit_horizon_ms, AT_LEAST_ONE),
            (
                "watch.confirm_rounds",
                u64::from(w.confirm_rounds),
                AT_LEAST_ONE,
            ),
            (
                "supervise.restart_window_ms",
                v.restart_window_ms,
                AT_LEAST_ONE,
            ),
            (
                "supervise.ready_timeout_ms",
                v.ready_timeout_ms,
                AT_LEAST_ONE,
            ),
            (
                "supervise.probe_interval_ms",
                v.probe_interval_ms,
                AT_LEAST_ONE,
            ),
            (
                "supervise.unresponsive_probes",
                u64::from(v.unresponsive_probes),
                AT_LEAST_ONE,
            ),
            (
                "supervise.backoff_factor",
                u64::from(v.backoff_factor),
                AT_LEAST_ONE,
            ),
        ] {
            if value == 0 {
                return invalid(key, &0, reason);
            }
        }
        Ok(())
    }

    /// Values that only mean something relative to each other.
    fn relations(&self) -> Result<(), ConfigError> {
        let (d, v) = (&self.discovery, &self.supervise);
        if !(d.learned_deadline_multiplier.is_finite() && d.learned_deadline_multiplier >= 1.0) {
            return invalid(
                "discovery.learned_deadline_multiplier",
                &d.learned_deadline_multiplier,
                "must be a finite number of at least 1: a deadline below the slowest recorded answer would call a healthy runtime slow",
            );
        }
        if d.learned_deadline_floor_ms > d.first_contact_timeout_ms {
            return invalid(
                "discovery.learned_deadline_floor_ms",
                &d.learned_deadline_floor_ms,
                "must not exceed discovery.first_contact_timeout_ms",
            );
        }
        if v.backoff_max_ms > v.restart_window_ms {
            return invalid(
                "supervise.backoff_max_ms",
                &v.backoff_max_ms,
                "must not exceed supervise.restart_window_ms, or a crash loop would never be noticed",
            );
        }
        if v.backoff_start_ms > v.backoff_max_ms {
            return invalid(
                "supervise.backoff_start_ms",
                &v.backoff_start_ms,
                "must not exceed supervise.backoff_max_ms",
            );
        }
        Ok(())
    }

    /// Deadlines are computed as now + a duration; one the clock cannot
    /// represent would make that overflow.
    fn representable(&self) -> Result<(), ConfigError> {
        let (v, w) = (&self.supervise, &self.watch);
        for (key, value) in [
            ("supervise.restart_window_ms", v.restart_window_ms),
            ("supervise.ready_timeout_ms", v.ready_timeout_ms),
            ("supervise.stop_grace_ms", v.stop_grace_ms),
            ("watch.doctor_interval_ms", w.doctor_interval_ms),
            ("watch.ports_interval_ms", w.ports_interval_ms),
        ] {
            let fits = std::time::Instant::now()
                .checked_add(std::time::Duration::from_millis(value))
                .is_some();
            if !fits {
                return invalid(key, &value, "is too large for this platform's clock");
            }
        }
        Ok(())
    }
}

/// Where the user's file is looked for, most specific first.
#[must_use]
pub fn user_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("HEARTH_CONFIG") {
        return Some(PathBuf::from(explicit));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".config")))?;
    Some(base.join("hearth").join("hearth.toml"))
}

/// Loads the base layer with the user's file merged over it. An explicitly
/// named file (`HEARTH_CONFIG`) must exist; the default location may not.
pub fn load() -> Result<Config, ConfigError> {
    let explicit = std::env::var_os("HEARTH_CONFIG").is_some();
    let overlay = match user_path() {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => Some((path.display().to_string(), text)),
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(ConfigError::Read { path, source }),
        },
        None => None,
    };
    from_layers(overlay.as_ref().map(|(o, t)| (o.as_str(), t.as_str())))
}

/// The base layer with an optional `(origin, text)` overlay.
pub fn from_layers(overlay: Option<(&str, &str)>) -> Result<Config, ConfigError> {
    let parse = |origin: &str, text: &str| {
        text.parse::<toml::Table>()
            .map_err(|source| ConfigError::Parse {
                origin: origin.to_string(),
                source,
            })
    };
    let mut table = parse("built-in config/hearth.toml", BASE)?;
    if let Some((origin, text)) = overlay {
        merge(&mut table, parse(origin, text)?);
    }
    let origin = overlay.map_or("built-in config/hearth.toml", |(o, _)| o);
    toml::Value::Table(table)
        .try_into::<Config>()
        .map_err(|source| ConfigError::Parse {
            origin: origin.to_string(),
            source,
        })?
        .validate()
}

fn merge(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_layer_parses() {
        from_layers(None).unwrap();
    }

    #[test]
    fn overlay_replaces_only_its_keys() {
        let base = from_layers(None).unwrap();
        let wanted = base.discovery.first_contact_timeout_ms + 1;
        let cfg = from_layers(Some((
            "test",
            &format!("[discovery]\nfirst_contact_timeout_ms = {wanted}\n"),
        )))
        .unwrap();
        assert_eq!(cfg.discovery.first_contact_timeout_ms, wanted);
    }

    #[test]
    fn meaningless_values_are_rejected() {
        for bad in [
            "[discovery]\nlearned_deadline_multiplier = 0.5\n",
            "[discovery]\nlearned_deadline_multiplier = nan\n",
            "[discovery]\nlearned_min_samples = 0\n",
            "[state]\nmax_snapshots_per_kind = 0\n",
            "[supervise]\nunresponsive_probes = 0\n",
            "[supervise]\nbackoff_start_ms = 9\nbackoff_max_ms = 8\n",
            "[supervise]\nbackoff_max_ms = 60001\n",
            "[supervise]\nbackoff_factor = 0\n",
            "[notify]\nwebhook_url = \"file:///etc/passwd\"\n",
            "[discovery]\nfirst_contact_timeout_ms = 10\nlearned_deadline_floor_ms = 11\n",
        ] {
            assert!(
                matches!(
                    from_layers(Some(("test", bad))),
                    Err(ConfigError::Invalid { .. })
                ),
                "{bad}"
            );
        }
    }

    /// What the clock can represent differs by platform, so the check is
    /// that a value either fails validation or can be used, never panics.
    #[test]
    fn huge_durations_are_rejected_or_usable() {
        let text = format!("[supervise]\nrestart_window_ms = {}\n", i64::MAX);
        if let Ok(cfg) = from_layers(Some(("test", &text))) {
            let _ = std::time::Instant::now()
                + std::time::Duration::from_millis(cfg.supervise.restart_window_ms);
        }
    }

    #[test]
    fn typos_are_rejected() {
        let err =
            from_layers(Some(("test", "[discovery]\nfirst_contact_timout_ms = 1\n"))).unwrap_err();
        assert!(err.to_string().contains("test"));
    }
}

/// One effective setting: its value, where that value came from, and what
/// the shipped file says it means.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Setting {
    pub key: String,
    /// `None` for an optional key nobody set.
    pub value: Option<toml::Value>,
    /// `built-in`, or the user file that set it.
    pub source: String,
    pub doc: String,
}

/// Comments in the shipped file, by `section.key`. A comment documents the
/// keys right below it, up to the next blank line, so keys that belong
/// together share one comment; a commented-out assignment (`# key = value`)
/// documents an optional key the same way.
fn docs() -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut section = String::new();
    let mut comment: Vec<&str> = Vec::new();
    let key_of = |line: &str| -> Option<String> {
        let (key, _) = line.split_once('=')?;
        let key = key.trim();
        (!key.is_empty() && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
            .then(|| key.to_string())
    };
    for line in BASE.lines().map(str::trim) {
        if line.is_empty() {
            comment.clear();
        } else if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_string();
            comment.clear();
        } else if let Some(text) = line.strip_prefix('#') {
            let text = text.trim();
            match key_of(text) {
                Some(key) if !section.is_empty() && !comment.is_empty() => {
                    out.insert(format!("{section}.{key}"), comment.join(" "));
                }
                _ => comment.push(text),
            }
        } else if let Some(key) = key_of(line) {
            out.insert(format!("{section}.{key}"), comment.join(" "));
        }
    }
    out
}

fn flatten(prefix: &str, table: &toml::Table, out: &mut Vec<(String, toml::Value)>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(t) => flatten(&path, t, out),
            v => out.push((path, v.clone())),
        }
    }
}

/// Every setting in effect, after the user's file has been merged and the
/// result validated, sorted by key.
pub fn explain() -> Result<Vec<Setting>, ConfigError> {
    load()?;
    let overlay = match user_path() {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => Some((path.display().to_string(), text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => return Err(ConfigError::Read { path, source }),
        },
        None => None,
    };
    let parse = |origin: &str, text: &str| {
        text.parse::<toml::Table>()
            .map_err(|source| ConfigError::Parse {
                origin: origin.to_string(),
                source,
            })
    };
    let mut base = Vec::new();
    flatten("", &parse("built-in config/hearth.toml", BASE)?, &mut base);
    let mut user = Vec::new();
    if let Some((origin, text)) = &overlay {
        flatten("", &parse(origin, text)?, &mut user);
    }
    let docs = docs();
    let user_value = |key: &str| user.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let origin = overlay.as_ref().map_or("", |(o, _)| o.as_str());
    let mut out: Vec<Setting> = base
        .iter()
        .map(|(key, value)| match user_value(key) {
            Some(v) => Setting {
                key: key.clone(),
                value: Some(v),
                source: origin.to_string(),
                doc: docs.get(key).cloned().unwrap_or_default(),
            },
            None => Setting {
                key: key.clone(),
                value: Some(value.clone()),
                source: "built-in".into(),
                doc: docs.get(key).cloned().unwrap_or_default(),
            },
        })
        .collect();
    // Optional keys: documented in the shipped file but not set there, and
    // keys only the user's file sets (validation has accepted them).
    for (key, doc) in &docs {
        if out.iter().any(|s| &s.key == key) {
            continue;
        }
        out.push(Setting {
            key: key.clone(),
            source: if user_value(key).is_some() {
                origin.to_string()
            } else {
                "unset".into()
            },
            value: user_value(key),
            doc: doc.clone(),
        });
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

#[cfg(test)]
mod explain_tests {
    use super::*;

    #[test]
    fn every_shipped_key_is_documented() {
        let mut keys = Vec::new();
        flatten("", &BASE.parse::<toml::Table>().unwrap(), &mut keys);
        let docs = docs();
        for (key, _) in keys {
            assert!(
                docs.get(&key).is_some_and(|d| !d.is_empty()),
                "{key} has no comment in config/hearth.toml"
            );
        }
        assert!(
            docs.contains_key("notify.webhook_url"),
            "optional keys are documented too"
        );
    }
}
