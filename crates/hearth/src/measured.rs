//! What starts of a model measured, from the run journal: the compute
//! buffers llama.cpp logged, and the footprint once it served.

use std::path::Path;

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

pub(crate) fn measured_compute_in(
    journal: &hearth_core::state::Journal,
    model: &Path,
    ctx: u64,
) -> (Option<u64>, Option<String>) {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let model = canon(model);
    let ids = match journal.ids(crate::run::KIND) {
        Ok(ids) => ids,
        Err(e) => return (None, Some(format!("run journal unreadable: {e}"))),
    };
    let mut elsewhere = None;
    let mut unreadable = 0usize;
    let mut found = None;
    for id in ids.iter().rev() {
        let Ok(s) = journal.read(id) else {
            unreadable += 1;
            continue;
        };
        let event = &s.body["event"];
        // Ready events of a start that logged compute buffers, of this model.
        let Some(compute) = event["compute_bytes"].as_u64().filter(|&c| c > 0) else {
            continue;
        };
        let identity = &event["identity"];
        if identity["model"].as_str().map(|m| canon(Path::new(m))) != Some(model.clone()) {
            continue;
        }
        let at = identity["n_ctx"]
            .as_u64()
            .map(|n| n.saturating_mul(identity["slots"].as_u64().unwrap_or(1).max(1)));
        if at == Some(ctx) {
            let footprint = s.body["footprint_at_ready"]["footprint_bytes"].as_u64();
            found = Some((compute, footprint, s.id));
            break;
        }
        elsewhere.get_or_insert((compute, at, s.id));
    }
    let skipped = if unreadable == 0 {
        String::new()
    } else {
        format!("; {unreadable} run events could not be read")
    };
    if let Some((compute, footprint, id)) = found {
        // Measured, not used in the verdict: whether a footprint counts the
        // memory-mapped weights is what the P0 gate (ADR-000) settles.
        let held = footprint.map_or_else(String::new, |f| {
            format!(
                "; at the moment it became ready, that server's footprint was {} (one reading; it may leave out memory-mapped weights, so it is not used in the verdict until ADR-000's gate says how they count)",
                bytes(f)
            )
        });
        return (
            Some(compute),
            Some(format!(
                "compute buffers {} as llama.cpp logged them when hearth started this model at this context ({id}), with that start's batch and flash-attention settings; without flash attention they grow with the context, so a larger context needs more{held}{skipped}",
                bytes(compute)
            )),
        );
    }
    let note = elsewhere
        .map(|(compute, at, id)| {
            format!(
                "compute buffers were {} at {} tokens ({id}); not counted at another context{skipped}",
                bytes(compute),
                at.map_or_else(|| "an unknown context".to_string(), |n| n.to_string())
            )
        })
        .or_else(|| (unreadable > 0).then(|| skipped.trim_start_matches("; ").to_string()));
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
        journal.record(crate::run::KIND, &ready(4096, 300)).unwrap();
        // A start that logged no compute buffers measured nothing.
        journal.record(crate::run::KIND, &ready(4096, 0)).unwrap();
        journal.record(crate::run::KIND, &ready(1024, 100)).unwrap();
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
}
