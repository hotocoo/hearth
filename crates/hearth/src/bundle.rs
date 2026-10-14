//! `hearth bundle`: everything hearth can say about this machine, in one
//! JSON document, for a bug report or an agent that needs the whole picture.
//!
//! Each part is the same command run with `--json`. A part that fails is
//! included as its error, so the bundle is always complete about what it
//! could not see. Secrets are left out: the webhook URL (it carries a
//! token) and server arguments (they can carry keys) are redacted.

use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Args {
    /// Incidents since this time prefix (e.g. 2026-10-14); all when unset.
    #[arg(long)]
    since: Option<String>,
}

/// Runs one hearth command line and returns its JSON, or its error.
fn part(argv: &[&str]) -> Value {
    let mut line = vec!["hearth".to_string(), "--json".to_string()];
    line.extend(argv.iter().map(|a| (*a).to_string()));
    match crate::dispatch(&line).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
        Ok(v) => v,
        Err(e) => json!({ "error": e }),
    }
}

/// Replaces values that may hold secrets.
fn redact(config: &mut Value) {
    for s in config["settings"].as_array_mut().into_iter().flatten() {
        let key = s["key"].as_str().unwrap_or_default().to_string();
        let secret = key == "notify.webhook_url"
            || (key.starts_with("servers.") && key.ends_with(".command"));
        if secret && !s["value"].is_null() {
            s["value"] = match (key.ends_with(".command"), s["value"].as_array()) {
                // The program says what runs; its arguments may hold keys.
                (true, Some(argv)) => json!([argv.first(), "(arguments redacted)"]),
                _ => json!("(redacted)"),
            };
        }
    }
}

pub fn run(args: &Args, _json: bool) -> Result<String, String> {
    let mut incidents = vec!["incidents", "--all"];
    if let Some(s) = &args.since {
        incidents.extend(["--since", s.as_str()]);
    }
    let mut config = part(&["config"]);
    redact(&mut config);
    let bundle = json!({
        "hearth_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "doctor": part(&["doctor"]),
        "ports": part(&["ports"]),
        "incidents": part(&incidents),
        "hearthd": part(&["status"]),
        "run_events": part(&["log", "--kind", "run", "--limit", "50"]),
        "tags": part(&["tag"]),
        "config": config,
    });
    crate::to_json(&bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted() {
        let mut config = json!({ "settings": [
            { "key": "notify.webhook_url", "value": "https://ntfy.sh/secret-token" },
            { "key": "servers.chat.command", "value": ["llama-server", "--api-key", "sk-1"] },
            { "key": "supervise.restart_budget", "value": 5 },
        ]});
        redact(&mut config);
        let text = config.to_string();
        assert!(
            !text.contains("secret-token") && !text.contains("sk-1"),
            "{text}"
        );
        assert!(text.contains("llama-server"));
        assert_eq!(config["settings"][2]["value"], 5);
    }
}
