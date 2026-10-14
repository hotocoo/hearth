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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    pub first_contact_timeout_ms: u64,
    pub max_probe_response_bytes: u64,
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
        .try_into()
        .map_err(|source| ConfigError::Parse {
            origin: origin.to_string(),
            source,
        })
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
    fn typos_are_rejected() {
        let err =
            from_layers(Some(("test", "[discovery]\nfirst_contact_timout_ms = 1\n"))).unwrap_err();
        assert!(err.to_string().contains("test"));
    }
}
