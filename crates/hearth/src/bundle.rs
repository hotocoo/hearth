//! `hearth bundle`: everything hearth can say about this machine, in one
//! JSON document, for a bug report or an agent that needs the whole picture.
//!
//! Each part is the same command run with `--json`. A part that fails is
//! included as its error, so the bundle is always complete about what it
//! could not see. Secrets are left out: the webhook URL (it carries a
//! token) and server arguments (they can carry keys) are redacted, and a
//! configuration that fails to load is reported without its error text,
//! which quotes the offending line. Free text from servers (their log
//! lines, in events and status) keeps its words but loses anything shaped
//! like a credential. Like the commands it runs, a bundle records fresh
//! ports and doctor snapshots.

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

/// Where a credential starts in free text: `key=VALUE` forms (URL query
/// strings, command lines) and the prefixes of common tokens (Hugging Face,
/// OpenAI-style, GitHub, Bearer headers). Lowercase.
const SECRET_MARKERS: &[&str] = &[
    "token=",
    "key=",
    "password=",
    "secret=",
    "bearer ",
    "hf_",
    "sk-",
    "ghp_",
    "github_pat_",
];

const SECRET_REDACTED: &str = "(redacted)";

/// `text` with the value after every [`SECRET_MARKERS`] marker replaced. A
/// token prefix counts only at the start of a word, so `disk-cache` stays.
fn redact_secrets(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let token_char = |c: char| c.is_ascii_alphanumeric() || "-_.~+/%".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        let at_word = !text[..i]
            .chars()
            .next_back()
            .is_some_and(|p| p.is_ascii_alphanumeric());
        let hit = SECRET_MARKERS
            .iter()
            .find(|m| lower[i..].starts_with(*m) && (m.ends_with(['=', ' ']) || at_word));
        if let Some(m) = hit {
            let start = i + m.len();
            let end = text[start..]
                .find(|c| !token_char(c))
                .map_or(text.len(), |n| start + n);
            // `HF_TOKEN=` names a variable; its value is what follows.
            let names_a_key = !m.ends_with(['=', ' ']) && text[end..].starts_with('=');
            if end > start && !names_a_key {
                out.push_str(&text[i..start]);
                out.push_str(SECRET_REDACTED);
                i = end;
                continue;
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Applies [`redact_secrets`] to every string in `value`.
fn redact_all(value: &mut Value) {
    match value {
        Value::String(s) => *s = redact_secrets(s),
        Value::Array(items) => items.iter_mut().for_each(redact_all),
        Value::Object(map) => map.values_mut().for_each(redact_all),
        _ => {}
    }
}

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
    // `hearth config` redacts what may hold secrets itself.
    let config = part(&["config"], c);
    let mut bundle = json!({
        "hearth_version": env!("CARGO_PKG_VERSION"),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "doctor": part(&["doctor"], c),
        "ports": part(&["ports"], c),
        "models": part(&["models"], c),
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
    redact_all(&mut bundle);
    crate::to_json(&bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_in_free_text_are_redacted_and_the_words_kept() {
        let line = "llama_model_load: GET https://hf.co/x?token=hf_abc123&x=1 failed: Bearer sk-live9 key=K9; disk-cache ok";
        assert_eq!(
            redact_secrets(line),
            "llama_model_load: GET https://hf.co/x?token=(redacted)&x=1 failed: Bearer (redacted) key=(redacted); disk-cache ok"
        );
        let mut v =
            json!({ "status": { "last_event": { "line": "HF_TOKEN=hf_x1 loaded" } }, "n": 1 });
        redact_all(&mut v);
        assert_eq!(
            v["status"]["last_event"]["line"],
            "HF_TOKEN=(redacted) loaded"
        );
        assert_eq!(v["n"], 1);
    }

    #[test]
    fn configuration_errors_are_scrubbed_everywhere() {
        let error = "h.toml: line 2: webhook_url = \"https://x/TOKEN\" oops";
        let mut v = json!({ "doctor": { "state_errors": [error] }, "n": 1 });
        scrub(&mut v, error);
        assert!(!v.to_string().contains("TOKEN"), "{v}");
        assert_eq!(v["n"], 1);
    }
}
