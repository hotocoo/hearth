//! `follow` (one process, port or server through every snapshot) and
//! `tag` (naming moments) over the journal.

use std::fmt::Write as _;

use hearth_core::state::{Journal, Snapshot};
use serde::Serialize;
use serde_json::Value;

use super::{FollowArgs, Kind, TagArgs, age, journal};

#[derive(Serialize)]
struct Step {
    id: String,
    taken_at_ms: u64,
    who: String,
    state: String,
}

/// The steps of everything matching `pattern` through `snapshots`, oldest
/// first: a step where a match appears, changes (or every snapshot with
/// `all`), and a `gone` step where it disappears.
///
/// With `events`, each snapshot is one event rather than a state of the
/// whole machine: every matching one is listed, and nothing is ever `gone`
/// because another snapshot is about something else.
fn timeline(
    snapshots: &[Snapshot],
    entries: impl Fn(&Value) -> Vec<(String, String)>,
    pattern: &str,
    (all, events): (bool, bool),
) -> Vec<Step> {
    let pattern = pattern.to_lowercase();
    let mut steps = Vec::new();
    let mut last: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for s in snapshots {
        let now: std::collections::BTreeMap<String, String> = entries(&s.body)
            .into_iter()
            .filter(|(who, _)| who.to_lowercase().contains(&pattern))
            .collect();
        let mut step = |who: &String, state: String| {
            steps.push(Step {
                id: s.id.clone(),
                taken_at_ms: s.taken_at_ms,
                who: who.clone(),
                state,
            });
        };
        for (who, state) in &now {
            if all || events || last.get(who) != Some(state) {
                step(who, state.clone());
            }
        }
        if events {
            continue;
        }
        for who in last.keys().filter(|w| !now.contains_key(*w)) {
            step(who, "gone".into());
        }
        last = now;
    }
    steps
}

/// The timeline `follow` shows, read from the newest snapshots back only as
/// far as `--limit` steps need: twice the limit, then twice that, until
/// enough steps or the whole history. Steps at the window's oldest snapshot
/// are not trusted (nothing before it was compared), unless the window
/// reaches the first snapshot.
fn latest_steps(journal: &Journal, args: &FollowArgs) -> Result<(Vec<Step>, Vec<String>), String> {
    let kind = args.kind.name();
    let total = journal.ids(kind).map_err(|e| e.to_string())?.len();
    let mut window = args.limit.map_or(usize::MAX, |l| l.saturating_mul(2));
    loop {
        let (mut snapshots, mut unreadable) = (Vec::new(), Vec::new());
        for snapshot in journal.recent(kind, window).map_err(|e| e.to_string())? {
            match snapshot {
                Ok(s) => snapshots.push(s),
                Err(e) => unreadable.push(e.to_string()),
            }
        }
        let events = matches!(args.kind, Kind::Run);
        let mut steps = timeline(
            &snapshots,
            |b| args.kind.entries(b),
            &args.pattern,
            (args.all, events),
        );
        let whole = window >= total;
        if !whole && let Some(oldest) = snapshots.first() {
            steps.retain(|s| s.id != oldest.id);
        }
        if whole || args.limit.is_some_and(|l| steps.len() >= l) {
            return Ok((steps, unreadable));
        }
        window = window.saturating_mul(2);
    }
}

/// `hearth follow`: what one process, port or server looked like in every
/// recorded snapshot, oldest first. By default only the snapshots where its
/// state changed, appeared or vanished are listed, the way `git log` lists
/// commits that touched a file.
pub fn follow(args: &FollowArgs, json: bool) -> Result<String, String> {
    let journal = journal()?;
    let (steps, unreadable) = latest_steps(&journal, args)?;
    let steps = &steps[steps.len().saturating_sub(args.limit.unwrap_or(usize::MAX))..];
    if json {
        return crate::to_json(&serde_json::json!({
            "kind": args.kind.name(),
            "pattern": args.pattern,
            "steps": steps,
            "unreadable": unreadable,
        }));
    }
    let mut out = String::new();
    if steps.is_empty() {
        let _ = writeln!(
            out,
            "nothing matching {:?} in the {} snapshots recorded",
            args.pattern,
            args.kind.name()
        );
    }
    for s in steps {
        let _ = writeln!(out, "{:>11}  {}: {}", age(s.taken_at_ms), s.who, s.state);
    }
    for e in &unreadable {
        let _ = writeln!(out, "unreadable: {e}");
    }
    Ok(out)
}

/// `hearth tag`: names the latest snapshot of every kind, like `git tag`, so
/// `diff --from NAME` can compare with that moment for as long as needed.
pub fn tag(args: &TagArgs, json: bool) -> Result<String, String> {
    let journal = journal()?;
    let kinds = [
        Kind::Ports.name(),
        Kind::Doctor.name(),
        Kind::Run.name(),
        Kind::Note.name(),
    ];
    let value = match (&args.name, args.delete) {
        (Some(name), true) => {
            journal.delete_tag(name).map_err(|e| e.to_string())?;
            serde_json::json!({ "deleted": name })
        }
        (Some(name), false) if args.at.is_some() => {
            let at = args.at.as_deref().unwrap_or_default();
            let id = journal
                .resolve(args.kind.name(), at)
                .map_err(|e| e.to_string())?
                .id;
            journal.tag_snapshot(name, &id).map_err(|e| e.to_string())?;
            serde_json::json!({ "tagged": name, "snapshots": { args.kind.name(): id } })
        }
        (Some(name), false) => {
            let ids = journal.set_tag(name, &kinds).map_err(|e| e.to_string())?;
            if ids.is_empty() {
                journal.delete_tag(name).map_err(|e| e.to_string())?;
                return Err(
                    "nothing recorded to tag yet: run hearth ports and hearth doctor".into(),
                );
            }
            serde_json::json!({ "tagged": name, "snapshots": ids })
        }
        (None, _) => {
            let (tags, errors) = journal.tags_lenient().map_err(|e| e.to_string())?;
            let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
            serde_json::json!({ "tags": tags, "unreadable": errors })
        }
    };
    if json {
        return crate::to_json(&value);
    }
    let mut out = String::new();
    if let Some(tags) = value["tags"].as_object() {
        if tags.is_empty() {
            let _ = writeln!(out, "no tags; hearth tag NAME names this moment");
        }
        for (name, ids) in tags {
            let _ = writeln!(out, "{name}: {ids}");
        }
        for e in value["unreadable"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "unreadable: {}", e.as_str().unwrap_or_default());
        }
    } else if let Some(name) = value["tagged"].as_str() {
        let named = value["snapshots"]
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(kind, id)| format!("{kind} {}", id.as_str().unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let which = if args.at.is_some() { "" } else { "the latest " };
        let _ = writeln!(
            out,
            "{name} now names {which}{named}; hearth diff --from {name} compares with it"
        );
    } else {
        let _ = writeln!(
            out,
            "removed tag {}",
            args.name.as_deref().unwrap_or_default()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(n: u64, ports: &[(&str, &str)]) -> Snapshot {
        Snapshot {
            id: format!("{n}-1-ports"),
            kind: "ports".into(),
            taken_at_ms: n,
            hearth_version: String::new(),
            body: serde_json::json!(ports),
        }
    }

    fn entries(body: &Value) -> Vec<(String, String)> {
        body.as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_str().unwrap().into(), p[1].as_str().unwrap().into()))
            .collect()
    }

    #[test]
    fn a_limited_follow_reads_back_only_as_far_as_it_needs_and_agrees() {
        let dir = std::env::temp_dir().join(format!("hearth-follow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let journal = Journal::at(dir.clone(), 100).unwrap();
        for n in 0..9 {
            // A port that changes state every third snapshot.
            let state = if (n / 3) % 2 == 0 { "ok" } else { "slow" };
            let ports = serde_json::json!({ "ports": [{ "addr": "127.0.0.1:1", "process": "p", "pid": 1, "runtime": { "kind": "llama_cpp", "health": state } }] });
            journal.record("ports", &ports).unwrap();
        }
        let args = |limit| FollowArgs {
            pattern: String::new(),
            kind: Kind::Ports,
            all: false,
            limit,
        };
        let (every, _) = latest_steps(&journal, &args(None)).unwrap();
        let (last, _) = latest_steps(&journal, &args(Some(1))).unwrap();
        assert!(!every.is_empty());
        assert_eq!(last.last().map(|s| &s.id), every.last().map(|s| &s.id));
        assert!(last.len() < every.len() || every.len() == 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn follow_lists_appearance_changes_and_disappearance_only() {
        let history = [
            snap(1, &[("ollama pid 5", "ok"), ("node pid 9", "ok")]),
            snap(2, &[("ollama pid 5", "ok")]),
            snap(3, &[("ollama pid 5", "NO ANSWER")]),
            snap(4, &[]),
        ];
        let got: Vec<(u64, String)> = timeline(&history, entries, "OLLAMA", (false, false))
            .into_iter()
            .map(|s| (s.taken_at_ms, s.state))
            .collect();
        assert_eq!(
            got,
            [
                (1, "ok".into()),
                (3, "NO ANSWER".into()),
                (4, "gone".into())
            ]
        );
        assert_eq!(
            timeline(&history, entries, "ollama", (true, false)).len(),
            4
        );
        // Events: every match, and another server's event is not a "gone".
        let events = [
            snap(1, &[("chat", "exited")]),
            snap(2, &[("vision", "ready")]),
            snap(3, &[("chat", "exited")]),
        ];
        let got: Vec<String> = timeline(&events, entries, "chat", (false, true))
            .into_iter()
            .map(|s| s.state)
            .collect();
        assert_eq!(got, ["exited", "exited"]);
    }
}
