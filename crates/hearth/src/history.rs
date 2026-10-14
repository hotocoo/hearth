//! `hearth log` and `hearth diff`: what hearth has observed on this machine
//! over time, and what changed between two observations.
//!
//! Commands that observe the whole machine record their report in hearth's
//! journal; these read it back. Nothing here probes or measures.

use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hearth_core::state::{Journal, Snapshot};
use serde::Serialize;
use serde_json::Value;

use crate::units::bytes;

/// Journal kinds `log` and `diff` understand.
#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Kind {
    /// Recorded by `hearth ports`.
    Ports,
    /// Recorded by `hearth doctor`.
    Doctor,
    /// Every event of every `hearth run`: starts, readiness, exits, wedges,
    /// poisonings, give-ups.
    Run,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Ports => crate::ports::KIND,
            Self::Doctor => crate::doctor::KIND,
            Self::Run => crate::run::KIND,
        }
    }

    fn summary(self, body: &Value) -> String {
        match self {
            Self::Ports => crate::ports::summary(body),
            Self::Doctor => crate::doctor::summary(body),
            Self::Run => crate::run::summary(body),
        }
    }

    fn entries(self, body: &Value) -> Vec<(String, String)> {
        match self {
            Self::Ports => crate::ports::entries(body),
            Self::Doctor => crate::doctor::entries(body),
            Self::Run => crate::run::entries(body),
        }
    }

    fn diff(self, before: &Value, after: &Value) -> Result<(Vec<Change>, Vec<Delta>), String> {
        match self {
            Self::Ports => Ok(crate::ports::diff(before, after)),
            Self::Doctor => Ok(crate::doctor::diff(before, after)),
            Self::Run => Err(
                "run snapshots are single events, not states; hearth log --kind run lists them"
                    .into(),
            ),
        }
    }
}

#[derive(clap::Args)]
pub struct LogArgs {
    #[arg(long, value_enum, default_value_t = Kind::Ports)]
    kind: Kind,
    /// Only the most recent N snapshots.
    #[arg(long)]
    limit: Option<NonZeroUsize>,
    /// Only snapshots from this long ago until now: 90s, 30m, 2h, 1d.
    #[arg(long, value_parser = parse_age)]
    since: Option<std::time::Duration>,
}

fn parse_age(text: &str) -> Result<std::time::Duration, String> {
    hearth_core::state::parse_age(text)
        .ok_or_else(|| format!("{text:?} is not a length of time like 90s, 30m, 2h or 1d"))
}

#[derive(clap::Args)]
pub struct DiffArgs {
    #[arg(long, value_enum, default_value_t = Kind::Ports)]
    kind: Kind,
    /// Older snapshot: an id from `hearth log`, ~N for N before the latest,
    /// @AGE for how things were that long ago (@2h, @1d), or a tag.
    #[arg(long, default_value = "~1")]
    from: String,
    /// Newer snapshot, same forms; ~0 is the latest.
    #[arg(long, default_value = "~0")]
    to: String,
    /// Only the N largest movements of each unit (bytes, counts), so byte
    /// deltas cannot crowd out a growing count of open files or threads.
    #[arg(long)]
    limit: Option<NonZeroUsize>,
}

#[derive(clap::Args)]
pub struct ShowArgs {
    #[arg(long, value_enum, default_value_t = Kind::Ports)]
    kind: Kind,
    /// An id from `hearth log`, ~N for N before the latest, @AGE (@2h), or
    /// a tag.
    #[arg(default_value = "~0")]
    reference: String,
}

#[derive(clap::Args)]
pub struct FollowArgs {
    /// Part of a process name, pid, address, path or server name; case is
    /// ignored.
    pattern: String,
    #[arg(long, value_enum, default_value_t = Kind::Ports)]
    kind: Kind,
    /// Show every snapshot, not only those where something changed.
    #[arg(long)]
    all: bool,
}

#[derive(clap::Args)]
pub struct TagArgs {
    /// Name for this moment (lowercase letters, digits, '-', '_', '.';
    /// starting with a letter). Without it, the tags are listed.
    name: Option<String>,
    /// Remove the tag instead; its snapshots are pruned like others again.
    #[arg(long, requires = "name")]
    delete: bool,
}

/// Something that is different between two snapshots.
#[derive(Serialize)]
pub struct Change {
    /// `appeared`, `vanished`, `restarted` or `changed`.
    pub kind: &'static str,
    pub subject: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Bytes,
    Count,
}

impl Unit {
    fn show(self, n: u64) -> String {
        match self {
            Self::Bytes => bytes(n),
            Self::Count => n.to_string(),
        }
    }
}

/// A measured quantity that moved between two snapshots.
#[derive(Serialize)]
pub struct Delta {
    pub subject: String,
    pub metric: &'static str,
    pub unit: Unit,
    pub before: u64,
    pub after: u64,
    pub by: i128,
}

impl Delta {
    pub fn new(subject: String, metric: &'static str, unit: Unit, before: u64, after: u64) -> Self {
        Self {
            subject,
            metric,
            unit,
            before,
            after,
            by: i128::from(after) - i128::from(before),
        }
    }
}

/// The closing lines of a recording command's human output.
pub fn footer(snapshot: Option<&str>, errors: &[String]) -> String {
    let mut out = String::new();
    if let Some(id) = snapshot {
        let _ = writeln!(
            out,
            "\nrecorded as {id}; hearth diff compares it with the run before"
        );
    }
    for e in errors {
        let _ = writeln!(out, "\nhistory: {e}");
    }
    out
}

fn journal() -> Result<Journal, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    Journal::open(config.state.max_snapshots_per_kind).map_err(|e| e.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// "42 s ago", "3 min ago", "5 h ago", "2 d ago".
fn age(taken_at_ms: u64) -> String {
    let secs = Duration::from_millis(now_ms().saturating_sub(taken_at_ms)).as_secs();
    let minute = Duration::from_secs(60).as_secs();
    let hour = minute * 60;
    let day = hour * 24;
    match secs {
        s if s < minute => format!("{s} s ago"),
        s if s < hour => format!("{} min ago", s / minute),
        s if s < day => format!("{} h ago", s / hour),
        s => format!("{} d ago", s / day),
    }
}

#[derive(Serialize)]
struct LogEntry {
    id: String,
    taken_at_ms: u64,
    hearth_version: String,
    summary: String,
}

#[derive(Serialize)]
struct LogReport {
    kind: &'static str,
    directory: String,
    /// Newest first.
    snapshots: Vec<LogEntry>,
    unreadable: Vec<String>,
}

pub fn log(args: &LogArgs, json: bool) -> Result<String, String> {
    let journal = journal()?;
    let kind = args.kind;
    let mut report = LogReport {
        kind: kind.name(),
        directory: journal.path().display().to_string(),
        snapshots: Vec::new(),
        unreadable: Vec::new(),
    };
    let limit = args.limit.map_or(usize::MAX, NonZeroUsize::get);
    let recent = match args.since {
        Some(age) => {
            let ids = journal
                .ids_since(kind.name(), age)
                .map_err(|e| e.to_string())?;
            let start = ids.len().saturating_sub(limit);
            ids[start..].iter().map(|id| journal.read(id)).collect()
        }
        None => journal
            .recent(kind.name(), limit)
            .map_err(|e| e.to_string())?,
    };
    for snapshot in recent.into_iter().rev() {
        match snapshot {
            Ok(s) => report.snapshots.push(LogEntry {
                summary: kind.summary(&s.body),
                id: s.id,
                taken_at_ms: s.taken_at_ms,
                hearth_version: s.hearth_version,
            }),
            Err(e) => report.unreadable.push(e.to_string()),
        }
    }
    if json {
        return crate::to_json(&report);
    }
    let mut out = String::new();
    if report.snapshots.is_empty() {
        let _ = writeln!(
            out,
            "no {} snapshots in {} yet: run hearth {}",
            report.kind, report.directory, report.kind
        );
    }
    for (back, s) in report.snapshots.iter().enumerate() {
        let _ = writeln!(
            out,
            "~{back:<3} {}  {:>11}  {}",
            s.id,
            age(s.taken_at_ms),
            s.summary
        );
    }
    for e in &report.unreadable {
        let _ = writeln!(out, "unreadable: {e}");
    }
    Ok(out)
}

#[derive(Serialize)]
struct Side {
    id: String,
    taken_at_ms: u64,
}

impl From<&Snapshot> for Side {
    fn from(s: &Snapshot) -> Self {
        Self {
            id: s.id.clone(),
            taken_at_ms: s.taken_at_ms,
        }
    }
}

#[derive(Serialize)]
struct DiffReport {
    kind: &'static str,
    from: Side,
    to: Side,
    changes: Vec<Change>,
    /// Grouped by unit, largest movement first within each.
    deltas: Vec<Delta>,
}

pub fn diff(args: &DiffArgs, json: bool) -> Result<String, String> {
    let journal = journal()?;
    let kind = args.kind;
    let resolve = |r: &str| journal.resolve(kind.name(), r).map_err(|e| e.to_string());
    let (from, to) = (resolve(&args.from)?, resolve(&args.to)?);
    let (changes, mut deltas) = kind.diff(&from.body, &to.body)?;
    deltas.retain(|d| d.by != 0);
    deltas.sort_by_key(|d| (d.unit, std::cmp::Reverse(d.by.unsigned_abs())));
    if let Some(limit) = args.limit {
        // Sorted by unit, so a running count per unit is enough.
        let mut seen: Option<(Unit, usize)> = None;
        deltas.retain(|d| {
            let n = match seen {
                Some((unit, n)) if unit == d.unit => n + 1,
                _ => 1,
            };
            seen = Some((d.unit, n));
            n <= limit.get()
        });
    }
    let report = DiffReport {
        kind: kind.name(),
        from: Side::from(&from),
        to: Side::from(&to),
        changes,
        deltas,
    };
    if json {
        crate::to_json(&report)
    } else {
        Ok(render_diff(&report))
    }
}

fn render_diff(r: &DiffReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} ({}) → {} ({})",
        r.kind,
        r.from.id,
        age(r.from.taken_at_ms),
        r.to.id,
        age(r.to.taken_at_ms)
    );
    if r.changes.is_empty() && r.deltas.is_empty() {
        let _ = writeln!(out, "no changes");
    }
    for c in &r.changes {
        let line = match (c.kind, &c.before, &c.after) {
            ("appeared", _, Some(after)) => format!("+ {}: {after}", c.subject),
            ("vanished", Some(before), _) => format!("- {}: {before}", c.subject),
            (change, before, after) => format!(
                "~ {} {change}: {} → {}",
                c.subject,
                before.as_deref().unwrap_or("-"),
                after.as_deref().unwrap_or("-")
            ),
        };
        let _ = writeln!(out, "{line}");
    }
    for d in &r.deltas {
        let sign = if d.by < 0 { "-" } else { "+" };
        let _ = writeln!(
            out,
            "  {} {}: {} → {} ({sign}{})",
            d.subject,
            d.metric,
            d.unit.show(d.before),
            d.unit.show(d.after),
            d.unit
                .show(u64::try_from(d.by.unsigned_abs()).unwrap_or(u64::MAX))
        );
    }
    out
}

/// `hearth show`: one recorded snapshot in full, as it was written.
pub fn show(args: &ShowArgs, _json: bool) -> Result<String, String> {
    let snapshot = journal()?
        .resolve(args.kind.name(), &args.reference)
        .map_err(|e| e.to_string())?;
    crate::to_json(&snapshot)
}

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

/// `hearth follow`: what one process, port or server looked like in every
/// recorded snapshot, oldest first. By default only the snapshots where its
/// state changed, appeared or vanished are listed, the way `git log` lists
/// commits that touched a file.
pub fn follow(args: &FollowArgs, json: bool) -> Result<String, String> {
    let journal = journal()?;
    let mut snapshots = Vec::new();
    let mut unreadable = Vec::new();
    for snapshot in journal
        .recent(args.kind.name(), usize::MAX)
        .map_err(|e| e.to_string())?
    {
        match snapshot {
            Ok(s) => snapshots.push(s),
            Err(e) => unreadable.push(e.to_string()),
        }
    }
    let events = matches!(args.kind, Kind::Run);
    let steps = timeline(
        &snapshots,
        |b| args.kind.entries(b),
        &args.pattern,
        (args.all, events),
    );
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
    for s in &steps {
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
    let kinds = [Kind::Ports.name(), Kind::Doctor.name(), Kind::Run.name()];
    let value = match (&args.name, args.delete) {
        (Some(name), true) => {
            journal.delete_tag(name).map_err(|e| e.to_string())?;
            serde_json::json!({ "deleted": name })
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
        let _ = writeln!(
            out,
            "{name} now names the latest {}; hearth diff --from {name} compares with it",
            value["snapshots"]
                .as_object()
                .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default()
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
