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
    /// Recorded by `hearth note`: what someone did or saw.
    Note,
}

impl Kind {
    const ALL: [Self; 4] = [Self::Ports, Self::Doctor, Self::Run, Self::Note];

    fn name(self) -> &'static str {
        match self {
            Self::Ports => crate::ports::KIND,
            Self::Doctor => crate::doctor::KIND,
            Self::Run => crate::run::KIND,
            Self::Note => crate::note::KIND,
        }
    }

    fn summary(self, body: &Value) -> String {
        match self {
            Self::Ports => crate::ports::summary(body),
            Self::Doctor => crate::doctor::summary(body),
            Self::Run => crate::run::summary(body),
            Self::Note => crate::note::summary(body),
        }
    }

    fn entries(self, body: &Value) -> Vec<(String, String)> {
        match self {
            Self::Ports => crate::ports::entries(body),
            Self::Doctor => crate::doctor::entries(body),
            Self::Run => crate::run::entries(body),
            Self::Note => crate::note::entries(body),
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
            Self::Note => {
                Err("notes are messages, not states; hearth log --kind note lists them".into())
            }
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
    /// Instead of listing: how much history of every kind is kept (count,
    /// time span, bytes), i.e. how far back references can reach.
    #[arg(long, conflicts_with_all = ["limit", "since", "grep"])]
    stats: bool,
    /// Only snapshots that mention this, like `git log -S`: a key or text
    /// value containing it (case ignored), or a number equal to it, such as
    /// a pid, model name, port, error or finding code. `--limit` then
    /// counts matches.
    #[arg(long)]
    grep: Option<String>,
    /// Every kind in one timeline, newest first, like `git log --all`:
    /// snapshots, run events and notes as they happened.
    #[arg(long, conflicts_with = "kind")]
    all: bool,
    /// Each snapshot in full, not only its summary, as JSON lines: one
    /// snapshot per line, oldest first, for archiving or attaching to a
    /// report (`hearth log --all --since 1d --full > day.jsonl`).
    #[arg(long, conflicts_with = "stats")]
    full: bool,
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
    /// Only this part of it, as a JSON pointer into the snapshot, like
    /// `git show REF:PATH`: `/body/health/memory`, `/body/ports/0`.
    #[arg(long)]
    pointer: Option<String>,
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
    /// Only the most recent N steps.
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(clap::Args)]
pub struct TagArgs {
    /// Name for this moment (lowercase letters, digits, '-', '_', '.';
    /// starting with a letter). Without it, the tags are listed.
    name: Option<String>,
    /// Remove the tag instead; its snapshots are pruned like others again.
    #[arg(long, requires = "name", conflicts_with = "at")]
    delete: bool,
    /// Name this recorded snapshot instead of the latest of every kind,
    /// like `git tag NAME COMMIT`: an id, ~N, @AGE or another tag, of
    /// --kind's snapshots (e.g. the first match of `hearth log --grep`).
    #[arg(long, requires = "name")]
    at: Option<String>,
    #[arg(long, value_enum, default_value_t = Kind::Ports, requires = "at")]
    kind: Kind,
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
    Journal::open(&config.state).map_err(|e| e.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// "42 s ago", "3 min ago", "5 h ago", "2 d ago".
pub(crate) fn age(taken_at_ms: u64) -> String {
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
    kind: &'static str,
    /// How far before the latest snapshot of its kind: `~back` refers to it.
    back: usize,
    id: String,
    taken_at_ms: u64,
    hearth_version: String,
    summary: String,
    /// The snapshot itself, with --full.
    #[serde(skip_serializing_if = "Option::is_none")]
    body: Option<Value>,
}

#[derive(Serialize)]
struct LogReport {
    kind: &'static str,
    directory: String,
    /// Newest first.
    snapshots: Vec<LogEntry>,
    unreadable: Vec<String>,
}

fn stats(journal: &Journal, json: bool) -> Result<String, String> {
    let mut kinds = Vec::new();
    for kind in Kind::ALL {
        let (count, oldest, newest, bytes) =
            journal.stats(kind.name()).map_err(|e| e.to_string())?;
        kinds.push(serde_json::json!({
            "kind": kind.name(), "snapshots": count,
            "oldest_ms": oldest, "newest_ms": newest, "bytes": bytes,
        }));
    }
    let (tags, bad) = journal.tags_lenient().map_err(|e| e.to_string())?;
    let tags = tags.len();
    let unreadable: Vec<String> = bad.iter().map(ToString::to_string).collect();
    let report = serde_json::json!({
        "directory": journal.path().display().to_string(),
        "kinds": kinds,
        "tags": tags,
        "unreadable_tags": unreadable,
    });
    if json {
        return crate::to_json(&report);
    }
    let mut out = format!("{}\n", journal.path().display());
    for k in &kinds {
        let span = match (k["oldest_ms"].as_u64(), k["newest_ms"].as_u64()) {
            (Some(o), Some(n)) => format!("from {} to {}", age(o), age(n)),
            _ => "nothing recorded".into(),
        };
        let _ = writeln!(
            out,
            "  {:<7} {:>5} snapshots, {span}, {}",
            k["kind"].as_str().unwrap_or_default(),
            k["snapshots"],
            bytes(k["bytes"].as_u64().unwrap_or_default())
        );
    }
    let _ = writeln!(out, "  {tags} tags");
    for e in &unreadable {
        let _ = writeln!(out, "  unreadable tag: {e}");
    }
    Ok(out)
}

/// Whether a recorded value mentions `needle`, as a person would read it:
/// text (keys and strings, unescaped) containing it with case ignored, or a
/// number equal to it, so pid 123 does not match 1234 or a byte count.
fn mentions(value: &Value, needle: &str) -> bool {
    let text = |t: &str| t.to_lowercase().contains(&needle.to_lowercase());
    match value {
        Value::String(t) => text(t),
        Value::Number(n) => n.to_string() == needle,
        Value::Bool(b) => b.to_string() == needle,
        Value::Array(items) => items.iter().any(|v| mentions(v, needle)),
        Value::Object(map) => map.iter().any(|(k, v)| text(k) || mentions(v, needle)),
        Value::Null => false,
    }
}

/// One self-contained line per snapshot, oldest first, so an archive
/// reads in the order things happened; the unreadable last.
fn json_lines(report: &LogReport) -> Result<String, String> {
    let mut out = String::new();
    for entry in report.snapshots.iter().rev() {
        let line = serde_json::to_string(entry).map_err(|e| format!("{}: {e}", entry.id))?;
        let _ = writeln!(out, "{line}");
    }
    // Same shape as a snapshot line, told apart by kind.
    for e in &report.unreadable {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({ "kind": "unreadable", "error": e })
        );
    }
    Ok(out)
}

pub fn log(args: &LogArgs, json: bool) -> Result<String, String> {
    let journal = journal()?;
    if args.stats {
        return stats(&journal, json);
    }
    let kinds: &[Kind] = if args.all {
        &Kind::ALL
    } else {
        std::slice::from_ref(&args.kind)
    };
    let mut report = LogReport {
        kind: if args.all { "all" } else { args.kind.name() },
        directory: journal.path().display().to_string(),
        snapshots: Vec::new(),
        unreadable: Vec::new(),
    };
    let limit = args.limit.map_or(usize::MAX, NonZeroUsize::get);
    // (kind, id, ~N within its kind), newest first across kinds: ids begin
    // with their time, zero-padded, so they sort in time order.
    let mut timeline = Vec::new();
    for &kind in kinds {
        let ids = match args.since {
            Some(age) => journal.ids_since(kind.name(), age),
            None => journal.ids(kind.name()),
        }
        .map_err(|e| e.to_string())?;
        timeline.extend(
            ids.into_iter()
                .rev()
                .enumerate()
                .map(|(back, id)| (kind, id, back)),
        );
    }
    timeline.sort_by(|a, b| b.1.cmp(&a.1));
    // Read lazily: with --grep, --limit counts matches and reading stops
    // at the last one needed.
    for (kind, id, back) in timeline {
        if report.snapshots.len() == limit {
            break;
        }
        match journal.read(&id) {
            // Thinned away between listing and reading: just gone.
            Err(hearth_core::state::StateError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Ok(s) if args.grep.as_deref().is_some_and(|n| !mentions(&s.body, n)) => {}
            Ok(s) => report.snapshots.push(LogEntry {
                kind: kind.name(),
                back,
                summary: kind.summary(&s.body),
                id: s.id,
                taken_at_ms: s.taken_at_ms,
                hearth_version: s.hearth_version,
                body: args.full.then_some(s.body),
            }),
            Err(e) => report.unreadable.push(e.to_string()),
        }
    }
    if args.full {
        return json_lines(&report);
    }
    if json {
        return crate::to_json(&report);
    }
    let mut out = String::new();
    if report.snapshots.is_empty() {
        let _ = match &args.grep {
            Some(g) if args.all => writeln!(out, "nothing recorded mentions {g:?}"),
            Some(g) => writeln!(out, "no {} snapshot mentions {g:?}", report.kind),
            None if args.all => writeln!(
                out,
                "nothing recorded in {} yet: run hearth ports or hearth doctor",
                report.directory
            ),
            None => writeln!(
                out,
                "no {} snapshots in {} yet: run hearth {}",
                report.kind, report.directory, report.kind
            ),
        };
    }
    for s in &report.snapshots {
        let kind = if args.all {
            format!("{:<7}", s.kind)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "{kind}~{:<3} {}  {:>11}  {}",
            s.back,
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
    /// What was noted (`hearth note`) and what supervised servers did
    /// between the two snapshots, oldest first: the why behind the what.
    meanwhile: Vec<LogEntry>,
    /// Events in that span that could not be read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unreadable: Vec<String>,
}

/// Notes and run events recorded after snapshot `from` and up to `to`,
/// oldest first; only the latest `limit` of each kind are read. Ids start
/// with their zero-padded time, so comparing ids compares times.
fn meanwhile(
    journal: &Journal,
    from: &str,
    to: &str,
    limit: Option<usize>,
) -> Result<(Vec<LogEntry>, Vec<String>), String> {
    let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
    let mut entries = Vec::new();
    let mut unreadable = Vec::new();
    for kind in [Kind::Run, Kind::Note] {
        let ids = journal.ids(kind.name()).map_err(|e| e.to_string())?;
        let count = ids.len();
        let within: Vec<(usize, &String)> = ids
            .iter()
            .enumerate()
            .filter(|(_, id)| id.as_str() > lo && id.as_str() <= hi)
            .collect();
        let skip = limit.map_or(0, |n| within.len().saturating_sub(n));
        for &(i, id) in &within[skip..] {
            match journal.read(id) {
                Ok(s) => entries.push(LogEntry {
                    kind: kind.name(),
                    back: count - 1 - i,
                    summary: kind.summary(&s.body),
                    id: s.id,
                    taken_at_ms: s.taken_at_ms,
                    hearth_version: s.hearth_version,
                    body: None,
                }),
                // Thinned away after listing: gone, not unreadable.
                Err(hearth_core::state::StateError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => unreadable.push(e.to_string()),
            }
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    Ok((entries, unreadable))
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
    let (mut meanwhile, unreadable) = meanwhile(
        &journal,
        &from.id,
        &to.id,
        args.limit.map(std::num::NonZero::get),
    )?;
    // --limit bounds these too, keeping the latest: a long span can hold
    // many events.
    if let Some(limit) = args.limit {
        let extra = meanwhile.len().saturating_sub(limit.get());
        meanwhile.drain(..extra);
    }
    let report = DiffReport {
        kind: kind.name(),
        from: Side::from(&from),
        to: Side::from(&to),
        changes,
        deltas,
        meanwhile,
        unreadable,
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
    if !r.meanwhile.is_empty() {
        let _ = writeln!(out, "meanwhile:");
    }
    for e in &r.meanwhile {
        let _ = writeln!(
            out,
            "  {:<4} {:>11}  {}",
            e.kind,
            age(e.taken_at_ms),
            e.summary
        );
    }
    for e in &r.unreadable {
        let _ = writeln!(out, "  unreadable: {e}");
    }
    out
}

/// `hearth show`: one recorded snapshot in full, as it was written.
pub fn show(args: &ShowArgs, _json: bool) -> Result<String, String> {
    let snapshot = journal()?
        .resolve(args.kind.name(), &args.reference)
        .map_err(|e| e.to_string())?;
    let Some(pointer) = &args.pointer else {
        return crate::to_json(&snapshot);
    };
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return Err(format!(
            "{pointer:?} is not a JSON pointer: it starts with /, as in /body/health"
        ));
    }
    let whole = serde_json::to_value(&snapshot).map_err(|e| e.to_string())?;
    let part = whole.pointer(pointer).ok_or_else(|| {
        format!(
            "nothing at {pointer} in {}; {}",
            snapshot.id,
            nearest(&whole, pointer)
        )
    })?;
    crate::to_json(part)
}

/// What is there at the deepest part of `pointer` that exists in `value`,
/// so a wrong path says where it went wrong.
fn nearest(value: &Value, pointer: &str) -> String {
    let mut at = "";
    for (i, _) in pointer.match_indices('/').skip(1) {
        if value.pointer(&pointer[..i]).is_none() {
            break;
        }
        at = &pointer[..i];
    }
    let here = match value.pointer(at) {
        Some(Value::Object(o)) => o.keys().cloned().collect::<Vec<_>>().join(", "),
        Some(Value::Array(a)) if a.is_empty() => "an empty list".to_string(),
        Some(Value::Array(a)) => format!("{} items, /0 to /{}", a.len(), a.len() - 1),
        Some(other) => format!("the value {other}"),
        None => String::new(),
    };
    format!("{} has: {here}", if at.is_empty() { "the top" } else { at })
}

mod moments;

pub use moments::{follow, tag};

#[cfg(test)]
mod tests;
