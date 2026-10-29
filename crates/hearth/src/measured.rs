//! What starts of a model measured, kept from the run journal's ready
//! events: the compute buffers llama.cpp logged, and the footprint once it
//! served.

use std::path::Path;

use hearth_core::state::{Journal, memo_key};
use serde_json::{Value, json};

use crate::units::bytes;

/// The compute buffers llama.cpp logged the last time hearth started this
/// model at `ctx` tokens, from the run journal's ready events, and a note
/// saying where the number came from (or why none was used).
pub(crate) fn measured_compute(model: &Path, ctx: u64) -> (Option<u64>, Option<String>) {
    match hearth_core::config::load()
        .map_err(|e| e.to_string())
        .and_then(|c| hearth_core::state::Journal::open(&c.state).map_err(|e| e.to_string()))
    {
        Ok(journal) => measured_compute_in(&journal, model, ctx),
        Err(e) => (
            None,
            Some(format!("no journal to learn compute buffers from: {e}")),
        ),
    }
}

/// Keeps what a ready event measured where the lookups below find it
/// without scanning the run journal: the server's footprint by its
/// definition, and the compute buffers by model and context. `id` is the
/// event's snapshot, which the notes cite.
pub(crate) fn remember_ready(journal: &Journal, id: &str, record: &Value) {
    if record["event"]["event"] != "ready" {
        return;
    }
    let footprint = record["footprint_at_ready"]["footprint_bytes"].as_u64();
    let mut memos = Vec::new();
    if let (Some(server), Some(definition), Some(f)) = (
        record["server"].as_str(),
        record["definition"].as_str(),
        footprint,
    ) {
        memos.push((
            memo_key("footprint", &(server, definition)),
            json!({ "footprint_bytes": f }),
        ));
    }
    let event = &record["event"];
    let identity = &event["identity"];
    if let (Some(compute), Some(model)) = (
        event["compute_bytes"].as_u64().filter(|&c| c > 0),
        identity["model"].as_str(),
    ) {
        let model = canon(Path::new(model));
        let at = identity["n_ctx"]
            .as_u64()
            .map(|n| n.saturating_mul(identity["slots"].as_u64().unwrap_or(1).max(1)));
        let measured = json!({ "compute": compute, "at": at, "footprint": footprint, "id": id });
        if let Some(at) = at {
            memos.push((memo_key("compute", &(&model, at)), measured.clone()));
        }
        memos.push((memo_key("compute-any", &model), measured));
    }
    for (key, value) in memos {
        if let Err(e) = journal.remember(&key, &value) {
            eprintln!("hearth: measurement not kept for lookup: {e}");
        }
    }
}

fn canon(p: &Path) -> std::path::PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The footprint server `name` had when it last became ready under
/// hearthd with this `definition` (`Server::definition`): a lower bound of
/// what it needs, since it may leave out memory-mapped weights (ADR-000). A
/// start of another model or context says nothing about this one.
pub(crate) fn last_footprint(journal: &Journal, name: &str, definition: &str) -> Option<u64> {
    journal.recall(&memo_key("footprint", &(name, definition)))?["footprint_bytes"].as_u64()
}

pub(crate) fn measured_compute_in(
    journal: &Journal,
    model: &Path,
    ctx: u64,
) -> (Option<u64>, Option<String>) {
    let model = canon(model);
    if let Some(m) = journal.recall(&memo_key("compute", &(&model, ctx)))
        && let Some(compute) = m["compute"].as_u64()
    {
        // Measured, not used in the verdict: whether a footprint counts the
        // memory-mapped weights is what the P0 gate (ADR-000) settles.
        let held = m["footprint"].as_u64().map_or_else(String::new, |f| {
            format!(
                "; at the moment it became ready, that server's footprint was {} (one reading; it may leave out memory-mapped weights, so it is not used in the verdict until ADR-000's gate says how they count)",
                bytes(f)
            )
        });
        return (
            Some(compute),
            Some(format!(
                "compute buffers {} as llama.cpp logged them when hearth started this model at this context ({}), with that start's batch and flash-attention settings; without flash attention they grow with the context, so a larger context needs more{held}",
                bytes(compute),
                m["id"].as_str().unwrap_or("?")
            )),
        );
    }
    let note = journal
        .recall(&memo_key("compute-any", &model))
        .and_then(|m| {
            Some(format!(
                "compute buffers were {} at {} tokens ({}); not counted at another context",
                bytes(m["compute"].as_u64()?),
                m["at"]
                    .as_u64()
                    .map_or_else(|| "an unknown context".to_string(), |n| n.to_string()),
                m["id"].as_str().unwrap_or("?")
            ))
        });
    (None, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_buffers_are_learned_from_a_ready_event_at_the_same_context() {
        let dir = std::env::temp_dir().join(format!("hearth-compute-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal = hearth_core::state::Journal::at(dir.clone(), 10).unwrap();
        let ready = |n_ctx: u64, compute: u64| {
            serde_json::json!({"event": {"event": "ready", "compute_bytes": compute,
                "identity": {"kind": "llama_cpp", "model": "/m/q.gguf", "n_ctx": n_ctx, "slots": 2}},
                "footprint_at_ready": {"footprint_bytes": 1_073_741_824_u64}})
        };
        let model = Path::new("/m/q.gguf");
        assert_eq!(measured_compute_in(&journal, model, 8192), (None, None));
        let keep = |r: &Value| {
            let id = journal.record(crate::run::KIND, r).unwrap().snapshot.id;
            remember_ready(&journal, &id, r);
        };
        keep(&ready(4096, 300));
        // A start that logged no compute buffers measured nothing.
        keep(&ready(4096, 0));
        keep(&ready(1024, 100));
        // 4096 per slot × 2 slots.
        let (compute, note) = measured_compute_in(&journal, model, 8192);
        assert_eq!(compute, Some(300));
        assert!(note.unwrap().contains("footprint was 1.00 GiB"));
        let (learned, note) = measured_compute_in(&journal, model, 4096);
        assert_eq!(learned, None);
        assert!(note.unwrap().contains("2048 tokens"));
        assert_eq!(
            measured_compute_in(&journal, Path::new("/other.gguf"), 8192),
            (None, None)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_footprint_counts_only_for_the_definition_it_was_measured_with() {
        let dir = std::env::temp_dir().join(format!("hearth-footprint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal = hearth_core::state::Journal::at(dir.clone(), 10).unwrap();
        let ready = |definition: &str, bytes: u64| {
            serde_json::json!({"server": "big", "definition": definition,
                "event": {"event": "ready"}, "footprint_at_ready": {"footprint_bytes": bytes}})
        };
        for r in [ready("70g", 70), ready("4g", 4)] {
            remember_ready(&journal, "id", &r);
        }
        assert_eq!(last_footprint(&journal, "big", "70g"), Some(70));
        assert_eq!(last_footprint(&journal, "big", "4g"), Some(4));
        assert_eq!(
            last_footprint(&journal, "big", "new"),
            None,
            "a changed server is unmeasured"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
