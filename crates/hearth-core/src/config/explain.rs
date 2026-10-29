//! What each setting means and where its value came from, for
//! `hearth config`: read from the comments in the shipped file.

use super::{BASE, ConfigError, load, user_path};

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

impl Setting {
    /// This setting with any value that may hold a secret replaced: the webhook URL
    /// (a token), a server's environment (where tokens go) and its arguments
    /// (they can carry keys); the program a server runs stays. Every output of
    /// this command is redacted, since agents read it too; the user file
    /// itself has the values.
    #[must_use]
    pub fn redacted(mut self) -> Self {
        let key = self.key.clone();
        let server = |part: &str| key.starts_with("servers.") && key.contains(part);
        let hidden = || toml::Value::String("(redacted)".into());
        self.value = self.value.map(|v| match v {
            toml::Value::Array(argv) if server(".command") => toml::Value::Array(
                argv.into_iter()
                    .take(1)
                    .chain([toml::Value::String("(arguments redacted)".into())])
                    .collect(),
            ),
            _ if key == "notify.webhook_url" || server(".env.") => hidden(),
            v => v,
        });
        self
    }
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
        } else if let Some(name) = line.strip_prefix("[[").and_then(|l| l.strip_suffix("]]")) {
            // An array of tables is one setting; its comment documents it,
            // and the keys inside its entries are not settings of their own.
            if !comment.is_empty() {
                out.entry(name.to_string())
                    .or_insert_with(|| comment.join(" "));
            }
            section.clear();
            comment.clear();
        } else if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // A comment right above a table documents the table itself
            // ([servers], whose entries the user names).
            if !comment.is_empty() {
                out.entry(name.to_string())
                    .or_insert_with(|| comment.join(" "));
            }
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
        } else if let Some(key) = key_of(line)
            && !section.is_empty()
        {
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

/// Every built-in setting with its default and meaning, as shipped.
pub fn explain_builtin() -> Result<Vec<Setting>, ConfigError> {
    let mut base = Vec::new();
    let table = BASE
        .parse::<toml::Table>()
        .map_err(|source| ConfigError::Parse {
            origin: "built-in config/hearth.toml".into(),
            source,
        })?;
    flatten("", &table, &mut base);
    let docs = docs();
    let mut out: Vec<Setting> = base
        .into_iter()
        .map(|(key, value)| Setting {
            doc: docs.get(&key).cloned().unwrap_or_default(),
            key,
            value: Some(value),
            source: "built-in".into(),
        })
        .collect();
    for (key, doc) in &docs {
        if !out.iter().any(|s| &s.key == key) {
            out.push(Setting {
                key: key.clone(),
                value: None,
                source: "unset".into(),
                doc: doc.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
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
    for (key, value) in user {
        if !out.iter().any(|s| s.key == key) {
            out.push(Setting {
                key,
                value: Some(value),
                source: origin.to_string(),
                doc: String::new(),
            });
        }
    }
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

#[cfg(test)]
mod tests {
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
