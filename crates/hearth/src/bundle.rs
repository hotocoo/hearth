//! `hearth bundle`: everything hearth can say about this machine, in one
//! JSON document, for a bug report or an agent that needs the whole picture.
//!
//! Each part is the same command run with `--json`. A part that fails is
//! included as its error, so the bundle is always complete about what it
//! could not see. Secrets are left out: the webhook URL (it carries a
//! token) and server arguments (they can carry keys) are redacted, and a
//! configuration that fails to load is reported without its error text,
//! which quotes the offending line. Like the commands it runs, a bundle
//! records fresh ports and doctor snapshots.

use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Args {
    /// Incidents since this time prefix (e.g. 2026-10-14); all when unset.
    #[arg(long)]
    since: Option<String>,
}

/// What a configuration error says, when the configuration fails to load:
/// such errors quote the offending line, which may be the webhook URL or a
/// server command with a key in it, so they never go into a bundle.
fn config_error() -> Option<String> {
    hearth_core::config::load().err().map(|e| e.to_string())
}

const NOTE_WITHHELD: &str =
    "(withheld: notes are free text and may quote secrets; hearth log --kind note shows them)";

const WITHHELD: &str = "the configuration does not load; details withheld from the bundle because they quote the offending line (run hearth config to see them)";

/// Runs one hearth command line and returns its JSON, or its error.
fn part(argv: &[&str], config_error: Option<&str>) -> Value {
    let mut line = vec!["hearth".to_string(), "--json".to_string()];
    line.extend(argv.iter().map(|a| (*a).to_string()));
    match crate::dispatch(&line).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
        Ok(v) => v,
        Err(e) => match config_error {
            Some(c) if e.contains(c) => json!({ "error": WITHHELD }),
            _ => json!({ "error": e }),
        },
    }
}

/// Replaces the configuration's error text wherever it appears in `value`:
/// commands that still succeed report it among their own errors.
fn scrub(value: &mut Value, error: &str) {
    match value {
        Value::String(s) if s.contains(error) => *s = WITHHELD.to_string(),
        Value::Array(items) => items.iter_mut().for_each(|v| scrub(v, error)),
        Value::Object(map) => map.values_mut().for_each(|v| scrub(v, error)),
        _ => {}
    }
}

/// Replaces values that may hold secrets.
fn redact(config: &mut Value) {
    for s in config["settings"].as_array_mut().into_iter().flatten() {
        let key = s["key"].as_str().unwrap_or_default().to_string();
        // Server variables are where tokens go (HF_TOKEN and the like).
        let secret = key == "notify.webhook_url"
            || (key.starts_with("servers.")
                && (key.ends_with(".command") || key.contains(".env.")));
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
    // A broken user file still gets a bundle, with the built-in count.
    let recent = hearth_core::config::load()
        .or_else(|_| hearth_core::config::from_layers(None))
        .map(|c| c.bundle.recent_events.to_string())
        .map_err(|e| e.to_string())?;
    let broken = config_error();
    let c = broken.as_deref();
    let mut config = part(&["config"], c);
    redact(&mut config);
    let mut bundle = json!({
        "hearth_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "doctor": part(&["doctor"], c),
        "ports": part(&["ports"], c),
        "incidents": part(&incidents, c),
        "hearthd": part(&["status"], c),
        "run_events": part(&["log", "--kind", "run", "--limit", &recent], c),
        "notes": part(&["log", "--kind", "note", "--limit", &recent], c),
        "tags": part(&["tag"], c),
        "config": config,
    });
    // Notes are free text, and agents write command lines into them, which
    // may hold keys: the bundle says when they were written, not what.
    for n in bundle["notes"]["snapshots"]
        .as_array_mut()
        .into_iter()
        .flatten()
    {
        n["summary"] = json!(NOTE_WITHHELD);
    }
    if let Some(error) = c {
        scrub(&mut bundle, error);
    }
    crate::to_json(&bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_errors_are_scrubbed_everywhere() {
        let error = "h.toml: line 2: webhook_url = \"https://x/TOKEN\" oops";
        let mut v = json!({ "doctor": { "state_errors": [error] }, "n": 1 });
        scrub(&mut v, error);
        assert!(!v.to_string().contains("TOKEN"), "{v}");
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn secrets_are_redacted() {
        let mut config = json!({ "settings": [
            { "key": "notify.webhook_url", "value": "https://ntfy.sh/secret-token" },
            { "key": "servers.chat.command", "value": ["llama-server", "--api-key", "sk-1"] },
            { "key": "servers.chat.env.HF_TOKEN", "value": "hf_secret" },
            { "key": "supervise.restart_budget", "value": 5 },
        ]});
        redact(&mut config);
        let text = config.to_string();
        assert!(
            !text.contains("secret-token") && !text.contains("sk-1") && !text.contains("hf_secret"),
            "{text}"
        );
        assert!(text.contains("llama-server"));
        assert_eq!(config["settings"][3]["value"], 5);
    }
}
