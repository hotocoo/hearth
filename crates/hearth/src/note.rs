//! `hearth note`: what someone did to this machine, in its journal beside
//! what hearth observed, like a commit message. "Restarted llama-server
//! with -c 131072" next to the doctor snapshots before and after it is
//! what makes the history readable later, by a person or an agent.

use std::fmt::Write as _;

use serde::Serialize;

/// The journal kind notes are recorded under.
pub const KIND: &str = "note";

#[derive(clap::Args)]
pub struct Args {
    /// Who wrote it: a person, or the agent acting for them. Give it
    /// before the message, which takes every word after it.
    #[arg(long)]
    by: Option<String>,
    /// What was done or seen, as words joined with spaces; flags in it
    /// (`-c 131072`) are part of the message.
    #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    message: Vec<String>,
}

#[derive(Serialize)]
struct Note<'a> {
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    by: Option<&'a str>,
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let message = args.message.join(" ");
    let message = message.trim();
    if message.is_empty() {
        return Err("a note needs some text".into());
    }
    let by = args.by.as_deref().map(str::trim).filter(|b| !b.is_empty());
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let journal = hearth_core::state::Journal::open(&config.state).map_err(|e| e.to_string())?;
    let recorded = journal
        .record(KIND, &Note { message, by })
        .map_err(|e| e.to_string())?;
    let warnings: Vec<String> = recorded.warnings.iter().map(ToString::to_string).collect();
    if json {
        return crate::to_json(&serde_json::json!({
            "id": recorded.snapshot.id,
            "warnings": warnings,
        }));
    }
    let mut out = format!(
        "noted as {}; hearth log --kind note lists notes\n",
        recorded.snapshot.id
    );
    for w in warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    Ok(out)
}

/// One line for `hearth log`.
pub fn summary(body: &serde_json::Value) -> String {
    let message = body["message"].as_str().unwrap_or("?");
    match body["by"].as_str() {
        Some(by) => format!("{by}: {message}"),
        None => message.to_string(),
    }
}

/// For `hearth follow --kind note note`: one subject, so each note shows
/// as the next change; `hearth log --kind note --grep WORD` filters them.
pub fn entries(body: &serde_json::Value) -> Vec<(String, String)> {
    vec![(KIND.to_string(), summary(body))]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_reads_as_its_author_and_message() {
        let body = serde_json::json!({"message": "restarted with -c 131072", "by": "claude"});
        assert_eq!(summary(&body), "claude: restarted with -c 131072");
        assert_eq!(summary(&serde_json::json!({"message": "m"})), "m");
        assert_eq!(entries(&body)[0].0, KIND);
    }
}
