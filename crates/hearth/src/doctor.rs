//! `hearth doctor`: the resources whose exhaustion takes a whole machine down.
//!
//! No thresholds are invented here. A finding is either something the kernel
//! itself declares (a memory pressure level) or a projection from measured
//! growth (`--watch`), stated with the time to the kernel's own limit.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use hearth_core::state::Journal;
use hearth_sense::{BoxHealth, PressureLevel, ProcessLoad};
use serde::Serialize;
use serde_json::Value;

use crate::history::{Change, Delta, Unit};
use crate::units::{bytes, percent};

/// The journal kind `doctor` records under.
pub const KIND: &str = "doctor";

#[derive(clap::Args)]
pub struct Args {
    /// Sample twice, this many seconds apart, and project when each growing
    /// resource would hit its limit at the measured rate.
    #[arg(long, value_name = "SECONDS")]
    watch: Option<u64>,
    /// Also say where every number comes from: the kernel counter, sysctl
    /// or file read, and the formula when one combines them.
    #[arg(long)]
    explain: bool,
}

/// One reported number and how it was obtained.
#[derive(Serialize)]
struct Derivation {
    quantity: &'static str,
    value: String,
    source: String,
}

/// Where a Linux pressure level comes from: PSI, against the thresholds.
fn linux_pressure_source(l: &hearth_sense::LinuxMem) -> String {
    match hearth_core::config::load() {
        Ok(c) => format!(
            "/proc/pressure/memory: some avg10 {:?} (warn at pressure.psi_some_warn = {}), full avg10 {:?} (critical at pressure.psi_full_critical = {})",
            l.psi_some_avg10,
            c.pressure.psi_some_warn,
            l.psi_full_avg10,
            c.pressure.psi_full_critical
        ),
        Err(_) => format!(
            "/proc/pressure/memory: some avg10 {:?}, full avg10 {:?}",
            l.psi_some_avg10, l.psi_full_avg10
        ),
    }
}

/// How each memory and limit figure in `h` was obtained on this platform.
fn derivations(h: &BoxHealth) -> Vec<Derivation> {
    let d = |quantity, value: String, source: String| Derivation {
        quantity,
        value,
        source,
    };
    let m = &h.memory;
    let macos = matches!(m.raw, hearth_sense::RawHost::Macos(_));
    let (total_from, swap_from) = if macos {
        ("sysctl hw.memsize", "sysctl vm.swapusage")
    } else {
        (
            "/proc/meminfo MemTotal",
            "/proc/meminfo SwapTotal - SwapFree",
        )
    };
    let level = m.pressure.map_or_else(
        || "not published".to_string(),
        |p| format!("{p:?}").to_lowercase(),
    );
    let mut out = vec![d("memory.total", bytes(m.total_bytes), total_from.into())];
    match &m.raw {
        hearth_sense::RawHost::Macos(vm) => {
            let page = |n: u64| bytes(n.saturating_mul(vm.page_size));
            out.push(d(
                "memory.available",
                bytes(m.available_bytes),
                format!(
                    "total - (app + wired + compressed), app = internal - purgeable, the Activity Monitor definition: {} - ({} - {} + {} + {}) from host_statistics64 (page size {})",
                    bytes(m.total_bytes),
                    page(vm.internal),
                    page(vm.purgeable),
                    page(vm.wired),
                    page(vm.compressor_occupied),
                    vm.page_size
                ),
            ));
            out.push(d(
                "memory.pressure",
                level.clone(),
                format!(
                    "sysctl kern.memorystatus_vm_pressure_level = {} (1 normal, 2 warn, 4 critical)",
                    vm.vm_pressure_level.map_or("unreadable".into(), |v| v.to_string())
                ),
            ));
        }
        hearth_sense::RawHost::Linux(l) => {
            out.push(d(
                "memory.available",
                bytes(m.available_bytes),
                "/proc/meminfo MemAvailable".into(),
            ));
            out.push(d(
                "memory.pressure",
                level.clone(),
                linux_pressure_source(l),
            ));
        }
    }
    out.push(d(
        "memory.swap_used",
        bytes(m.swap_used_bytes),
        swap_from.into(),
    ));
    let l = &h.limits;
    let shown = |v: Option<u64>| v.map_or("unknown".into(), |n| n.to_string());
    for (field, value) in [
        ("files_open", l.files_open),
        ("threads_total", l.threads_total),
        ("files_max", l.files_max),
        ("files_per_process_max", l.files_per_process_max),
        ("threads_max", l.threads_max),
        ("threads_per_process_max", l.threads_per_process_max),
        ("processes_max", l.processes_max),
    ] {
        let source = hearth_sense::LIMIT_SOURCES
            .iter()
            .find(|(f, _)| *f == field)
            .map_or("not measured on this platform", |(_, s)| s);
        out.push(d(
            hearth_sense::LIMIT_SOURCES
                .iter()
                .find(|(f, _)| *f == field)
                .map_or(field, |(f, _)| f),
            shown(value),
            source.to_string(),
        ));
    }
    out
}

#[derive(Serialize)]
struct Report {
    health: BoxHealth,
    /// Fewest processes that together hold at least half of all open files.
    file_majority: Vec<Holder>,
    /// Fewest processes that together hold at least half of all threads.
    thread_majority: Vec<Holder>,
    /// Processes holding TCP sockets in `CLOSE_WAIT`, most first.
    close_wait: Vec<Holder>,
    /// Process with the largest share of its own open-file limit.
    nearest_file_limit: Option<Holder>,
    trend: Option<Trend>,
    findings: Vec<Finding>,
    /// With `--explain`: where every number came from.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    explanation: Vec<Derivation>,
    /// Journal id this report was recorded under.
    snapshot: Option<String>,
    /// Why the report could not be recorded; the report is still complete.
    state_errors: Vec<String>,
}

#[derive(Serialize, Clone)]
struct Holder {
    pid: i32,
    name: String,
    count: u64,
}

#[derive(Serialize)]
struct Trend {
    seconds: f64,
    files_per_second: Option<f64>,
    threads_per_second: f64,
    growing: Vec<Growth>,
}

#[derive(Serialize)]
struct Growth {
    pid: i32,
    name: String,
    resource: &'static str,
    per_second: f64,
    now: u64,
    limit: Option<u64>,
    seconds_to_limit: Option<f64>,
}

#[derive(Serialize)]
struct Finding {
    severity: &'static str,
    /// What kind of finding this is, stable across runs: with `pid`, what
    /// `hearth watch` uses to tell a new problem from one it already reported.
    code: &'static str,
    pid: Option<i32>,
    message: String,
    /// What to do about it, for a person or an agent.
    remediation: String,
}

/// A problem `hearth watch` reports once when it appears and once when it
/// clears. `key` says what it is about, stable across observations.
pub struct Alarm {
    pub key: String,
    pub text: String,
    /// Whether it has to hold for `watch.confirm_rounds` rounds before it is
    /// reported: true for projections from one measured rate, which wobble;
    /// false for what the kernel itself declares.
    pub confirm: bool,
}

/// One doctor sample, kept so the next one can be compared with it.
pub struct Observation {
    report: Report,
    at: Instant,
}

impl Observation {
    /// Problems a person should hear about, each with what to do:
    /// * memory pressure the kernel declares (warn or critical);
    /// * any resource that would reach its limit within `horizon` at the
    ///   growth rate measured since the previous observation.
    ///
    /// Growth alone is not a problem: thread and descriptor counts move all
    /// the time.
    pub fn alarms(&self, horizon: Duration) -> Vec<Alarm> {
        let r = &self.report;
        let mut out: Vec<_> = r
            .findings
            .iter()
            .filter(|f| f.code == "memory_pressure")
            // Keyed by level, so warn rising to critical is reported anew.
            .map(|f| Alarm {
                key: format!("{}:{}", f.code, f.severity),
                text: format!("[{}] {}; {}", f.severity, f.message, f.remediation),
                confirm: false,
            })
            .collect();
        let Some(t) = &r.trend else { return out };
        let soon = |secs: f64| secs.is_finite() && secs < horizon.as_secs_f64();
        if let (Some(rate), Some(open), Some(max)) = (
            t.files_per_second,
            r.health.limits.files_open,
            r.health.limits.files_max,
        ) && rate > 0.0
        {
            #[allow(clippy::cast_precision_loss)]
            let secs = max.saturating_sub(open) as f64 / rate;
            if soon(secs) {
                out.push(Alarm {
                    key: "files_growing".into(),
                    confirm: true,
                    text: format!(
                        "open files on this machine growing {rate:.1}/s: the kernel limit ({max}) in {}; find the leaking process with hearth diff --kind doctor",
                        duration(secs)
                    ),
                });
            }
        }
        for g in &t.growing {
            if let Some(secs) = g.seconds_to_limit.filter(|&s| soon(s)) {
                out.push(Alarm {
                    key: format!("{}:{}", g.resource, g.pid),
                    confirm: true,
                    text: format!(
                        "{} (pid {}) {} growing {:.1}/s: its limit in {}; restart it or fix the leak before then",
                        g.name,
                        g.pid,
                        g.resource,
                        g.per_second,
                        duration(secs)
                    ),
                });
            }
        }
        out
    }

    pub fn state_errors(&self) -> &[String] {
        &self.report.state_errors
    }
}

/// Measures the machine, with a trend from `previous` when given, and
/// records the report.
pub fn observe(previous: Option<&Observation>) -> Result<Observation, String> {
    let health = crate::pressure::box_health()?;
    let at = Instant::now();
    let trend = previous.map(|p| {
        trend(
            &p.report.health,
            &health,
            at.duration_since(p.at).as_secs_f64(),
        )
    });
    let mut report = build(health, trend);
    record(&mut report);
    Ok(Observation { report, at })
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let first = crate::pressure::box_health()?;
    let (health, trend) = match args.watch {
        Some(secs) => {
            let started = Instant::now();
            std::thread::sleep(Duration::from_secs(secs));
            let second = crate::pressure::box_health()?;
            let elapsed = started.elapsed().as_secs_f64();
            let trend = trend(&first, &second, elapsed);
            (second, Some(trend))
        }
        None => (first, None),
    };
    let mut report = build(health, trend);
    if args.explain {
        report.explanation = derivations(&report.health);
    }
    record(&mut report);
    if json {
        crate::to_json(&report)
    } else {
        let mut out = render(&report);
        if !report.explanation.is_empty() {
            out.push_str("\nwhere the numbers come from:\n");
            for e in &report.explanation {
                let _ = writeln!(out, "  {} = {}\n      {}", e.quantity, e.value, e.source);
            }
        }
        Ok(out + &crate::history::footer(report.snapshot.as_deref(), &report.state_errors))
    }
}

fn record(report: &mut Report) {
    let journal = hearth_core::config::load()
        .map_err(|e| e.to_string())
        .and_then(|c| Journal::open(&c.state).map_err(|e| e.to_string()));
    match journal.and_then(|j| j.record(KIND, &*report).map_err(|e| e.to_string())) {
        Ok(r) => {
            report.snapshot = Some(r.snapshot.id);
            report
                .state_errors
                .extend(r.warnings.iter().map(ToString::to_string));
        }
        Err(e) => report.state_errors.push(e),
    }
}

/// Each process in a recorded report as `(who, how it was)`, for
/// `hearth follow`: name and pid; open files, threads and `CLOSE_WAIT`
/// sockets.
pub fn entries(body: &Value) -> Vec<(String, String)> {
    let count = |v: &Value| v.as_u64().map_or("?".into(), |n| n.to_string());
    body["health"]["processes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            (
                format!("{} pid {}", p["name"].as_str().unwrap_or("?"), p["pid"]),
                format!(
                    "open files {}, threads {}, CLOSE_WAIT {}",
                    count(&p["open_files"]),
                    count(&p["threads"]),
                    count(&p["close_wait"])
                ),
            )
        })
        .collect()
}

/// One line for `hearth log`.
pub fn summary(body: &Value) -> String {
    let h = &body["health"];
    let of = |used: &Value, max: &Value| match (used.as_u64(), max.as_u64()) {
        (Some(u), Some(m)) => format!("{u}/{m}"),
        (Some(u), None) => u.to_string(),
        _ => "?".into(),
    };
    format!(
        "available {}, pressure {}, files {}, threads {}, {} findings",
        h["memory"]["available_bytes"]
            .as_u64()
            .map_or("?".into(), bytes),
        h["memory"]["pressure"].as_str().unwrap_or("?"),
        of(&h["limits"]["files_open"], &h["limits"]["files_max"]),
        of(&h["limits"]["threads_total"], &h["limits"]["threads_max"]),
        body["findings"].as_array().map_or(0, Vec::len)
    )
}

/// How host memory, kernel-wide counts and each surviving process's open
/// files, threads and `CLOSE_WAIT` sockets moved between two recorded
/// reports. Processes are matched by pid and name, so a reused pid is not
/// mistaken for growth.
pub fn diff(before: &Value, after: &Value) -> (Vec<Change>, Vec<Delta>) {
    let (b, a) = (&before["health"], &after["health"]);
    let mut changes = Vec::new();
    let (was, now) = (&b["memory"]["pressure"], &a["memory"]["pressure"]);
    if was != now {
        changes.push(Change {
            kind: "changed",
            subject: "memory pressure".into(),
            before: was.as_str().map(str::to_string),
            after: now.as_str().map(str::to_string),
        });
    }
    let mut deltas = Vec::new();
    for (section, field, metric, unit) in [
        ("memory", "available_bytes", "available", Unit::Bytes),
        ("memory", "swap_used_bytes", "swap used", Unit::Bytes),
        ("limits", "files_open", "open files", Unit::Count),
        ("limits", "threads_total", "threads", Unit::Count),
    ] {
        if let (Some(x), Some(y)) = (b[section][field].as_u64(), a[section][field].as_u64()) {
            deltas.push(Delta::new("machine".into(), metric, unit, x, y));
        }
    }
    let processes = |h: &Value| -> BTreeMap<(i64, String), Value> {
        h["processes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| {
                Some((
                    (p["pid"].as_i64()?, p["name"].as_str()?.to_string()),
                    p.clone(),
                ))
            })
            .collect()
    };
    let earlier = processes(b);
    for ((pid, name), p) in processes(a) {
        let Some(old) = earlier.get(&(pid, name.clone())) else {
            continue;
        };
        for (field, metric) in [
            ("open_files", "open files"),
            ("threads", "threads"),
            ("close_wait", "CLOSE_WAIT sockets"),
        ] {
            if let (Some(x), Some(y)) = (old[field].as_u64(), p[field].as_u64()) {
                deltas.push(Delta::new(
                    format!("{name} ({pid})"),
                    metric,
                    Unit::Count,
                    x,
                    y,
                ));
            }
        }
    }
    (changes, deltas)
}

fn majority(processes: &[ProcessLoad], count: impl Fn(&ProcessLoad) -> Option<u64>) -> Vec<Holder> {
    let mut held: Vec<Holder> = processes
        .iter()
        .filter_map(|p| {
            Some(Holder {
                pid: p.pid,
                name: p.name.clone(),
                count: count(p)?,
            })
        })
        .collect();
    held.sort_by_key(|h| std::cmp::Reverse(h.count));
    let total: u64 = held.iter().map(|h| h.count).sum();
    let mut running = 0;
    held.into_iter()
        .take_while(|h| {
            let needed = running * 2 < total;
            running += h.count;
            needed
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn trend(a: &BoxHealth, b: &BoxHealth, seconds: f64) -> Trend {
    let before: BTreeMap<i32, &ProcessLoad> = a.processes.iter().map(|p| (p.pid, p)).collect();
    let per_process_files = b.limits.files_per_process_max;
    let per_process_threads = b.limits.threads_per_process_max;
    let mut growing = Vec::new();
    for p in &b.processes {
        let Some(old) = before.get(&p.pid) else {
            continue;
        };
        for (resource, now, then, limit) in [
            (
                "open_files",
                p.open_files,
                old.open_files,
                per_process_files,
            ),
            ("threads", p.threads, old.threads, per_process_threads),
            // CLOSE_WAIT sockets are open files, so they share that limit.
            (
                "close_wait",
                p.close_wait,
                old.close_wait,
                per_process_files,
            ),
        ] {
            let (Some(now), Some(then)) = (now, then) else {
                continue;
            };
            if now <= then {
                continue;
            }
            let per_second = (now - then) as f64 / seconds;
            growing.push(Growth {
                pid: p.pid,
                name: p.name.clone(),
                resource,
                per_second,
                now,
                limit,
                seconds_to_limit: limit.map(|l| l.saturating_sub(now) as f64 / per_second),
            });
        }
    }
    growing.sort_by(|x, y| {
        x.seconds_to_limit
            .unwrap_or(f64::INFINITY)
            .total_cmp(&y.seconds_to_limit.unwrap_or(f64::INFINITY))
    });
    let files_per_second = a
        .limits
        .files_open
        .zip(b.limits.files_open)
        .map(|(x, y)| (y as f64 - x as f64) / seconds);
    Trend {
        seconds,
        files_per_second,
        threads_per_second: match (a.limits.threads_total, b.limits.threads_total) {
            (Some(x), Some(y)) => (y as f64 - x as f64) / seconds,
            _ => (b.threads_counted as f64 - a.threads_counted as f64) / seconds,
        },
        growing,
    }
}

/// What the kernel's own memory pressure level says.
fn pressure(level: Option<PressureLevel>) -> Option<Finding> {
    match level {
        Some(PressureLevel::Warn) => Some(Finding {
            severity: "warn",
            code: "memory_pressure",
            pid: None,
            message: "memory pressure: warn".into(),
            remediation: "do not load more models; unload idle ones or reduce context sizes".into(),
        }),
        Some(PressureLevel::Critical) => Some(Finding {
            severity: "critical",
            code: "memory_pressure",
            pid: None,
            message: "memory pressure: critical".into(),
            remediation:
                "free memory now: stop the largest idle model server; jetsam kills are imminent"
                    .into(),
        }),
        Some(PressureLevel::Normal) | None => None,
    }
}

#[allow(clippy::cast_precision_loss)]
fn build(health: BoxHealth, trend: Option<Trend>) -> Report {
    let file_majority = majority(&health.processes, |p| p.open_files);
    let thread_majority = majority(&health.processes, |p| p.threads);
    let mut close_wait: Vec<Holder> = health
        .processes
        .iter()
        .filter_map(|p| {
            Some(Holder {
                pid: p.pid,
                name: p.name.clone(),
                count: p.close_wait.filter(|&n| n > 0)?,
            })
        })
        .collect();
    close_wait.sort_by_key(|h| std::cmp::Reverse(h.count));
    let nearest_file_limit = health.limits.files_per_process_max.and_then(|limit| {
        health
            .processes
            .iter()
            .filter_map(|p| Some((p, p.open_files?)))
            .max_by_key(|(_, n)| *n)
            .filter(|_| limit > 0)
            .map(|(p, n)| Holder {
                pid: p.pid,
                name: p.name.clone(),
                count: n,
            })
    });
    let mut findings: Vec<Finding> = pressure(health.memory.pressure).into_iter().collect();
    if let Some(t) = &trend {
        if let (Some(rate), Some(open), Some(max)) = (
            t.files_per_second,
            health.limits.files_open,
            health.limits.files_max,
        ) && rate > 0.0
        {
            findings.push(Finding {
                severity: "warn",
                code: "files_growing",
                pid: None,
                message: format!(
                    "system open files growing {rate:.1}/s; limit reached in {} at this rate",
                    duration(max.saturating_sub(open) as f64 / rate)
                ),
                remediation: "see trend.growing for the processes leaking descriptors".into(),
            });
        }
        for g in t.growing.iter().filter(|g| g.seconds_to_limit.is_some()) {
            if let Some(secs) = g.seconds_to_limit {
                findings.push(Finding {
                    severity: "info",
                    code: g.resource,
                    pid: Some(g.pid),
                    message: format!(
                        "{} (pid {}) {} growing {:.1}/s; its limit in {}",
                        g.name,
                        g.pid,
                        g.resource,
                        g.per_second,
                        duration(secs)
                    ),
                    remediation: if g.resource == "close_wait" {
                        format!(
                            "{} is not closing sockets after clients disconnect; fix the handler (close or destroy the upstream on client close) or restart it",
                            g.name
                        )
                    } else {
                        format!("if this continues, restart {} or fix the leak", g.name)
                    },
                });
            }
        }
    }
    Report {
        health,
        file_majority,
        thread_majority,
        close_wait,
        nearest_file_limit,
        trend,
        findings,
        explanation: Vec::new(),
        snapshot: None,
        state_errors: Vec::new(),
    }
}

fn duration(seconds: f64) -> String {
    if !seconds.is_finite() {
        return "never".into();
    }
    let s = seconds.round();
    if s < 60.0 {
        format!("{s:.0}s")
    } else if s < 3600.0 {
        format!("{:.0}m", s / 60.0)
    } else if s < 86_400.0 {
        format!("{:.1}h", s / 3600.0)
    } else {
        format!("{:.1}d", s / 86_400.0)
    }
}

fn holders(list: &[Holder]) -> String {
    list.iter()
        .map(|h| format!("{} ({}) {}", h.name, h.pid, h.count))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render(r: &Report) -> String {
    let m = &r.health.memory;
    let l = &r.health.limits;
    let mut out = String::new();
    let pressure = m.pressure.map_or("not published", |p| match p {
        PressureLevel::Normal => "normal",
        PressureLevel::Warn => "WARN",
        PressureLevel::Critical => "CRITICAL",
    });
    let _ = writeln!(
        out,
        "memory   {} available of {} · pressure {pressure} · swap {} of {} used",
        bytes(m.available_bytes),
        bytes(m.total_bytes),
        bytes(m.swap_used_bytes),
        bytes(m.swap_total_bytes),
    );
    if let (Some(open), Some(max)) = (l.files_open, l.files_max) {
        let _ = writeln!(
            out,
            "files    {open} open of {max} ({}) · per-process limit {}",
            percent(open, max),
            l.files_per_process_max
                .map_or("unknown".into(), |n| n.to_string())
        );
    }
    match l.threads_total {
        Some(total) => {
            let _ = writeln!(
                out,
                "threads  {total} total of {} ({}) · {} in processes readable here, {} in {} processes owned by other users",
                l.threads_max.map_or("unknown".into(), |n| n.to_string()),
                l.threads_max.map_or("n/a".into(), |m| percent(total, m)),
                r.health.threads_counted,
                total.saturating_sub(r.health.threads_counted),
                r.health.uninspectable
            );
        }
        None => {
            let _ = writeln!(
                out,
                "threads  at least {} (system total unavailable; {} processes not inspectable) · limit {}",
                r.health.threads_counted,
                r.health.uninspectable,
                l.threads_max.map_or("unknown".into(), |n| n.to_string()),
            );
        }
    }
    let _ = writeln!(
        out,
        "\nhalf of all open files: {}",
        holders(&r.file_majority)
    );
    let _ = writeln!(
        out,
        "half of all threads:    {}",
        holders(&r.thread_majority)
    );
    if !r.close_wait.is_empty() {
        let _ = writeln!(out, "CLOSE_WAIT sockets:     {}", holders(&r.close_wait));
    }
    if let (Some(h), Some(limit)) = (&r.nearest_file_limit, l.files_per_process_max) {
        let _ = writeln!(
            out,
            "closest to its file limit: {} ({}) {} of {} ({})",
            h.name,
            h.pid,
            h.count,
            limit,
            percent(h.count, limit)
        );
    }
    if let Some(t) = &r.trend {
        let _ = writeln!(
            out,
            "\nover {:.0}s: files {}/s, threads {:+.1}/s",
            t.seconds,
            t.files_per_second
                .map_or("n/a".into(), |f| format!("{f:+.1}")),
            t.threads_per_second
        );
    }
    if r.findings.is_empty() {
        let _ = writeln!(out, "\nno findings");
    } else {
        let _ = writeln!(out, "\nfindings:");
        for f in &r.findings {
            let _ = writeln!(
                out,
                "  [{}] {}\n         → {}",
                f.severity, f.message, f.remediation
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(pid: i32, files: u64) -> ProcessLoad {
        ProcessLoad {
            pid,
            name: format!("p{pid}"),
            open_files: Some(files),
            threads: None,
            close_wait: None,
            listening: Vec::new(),
        }
    }

    #[test]
    fn majority_is_the_fewest_processes_holding_half() {
        let procs = [load(1, 10), load(2, 60), load(3, 20), load(4, 10)];
        let m = majority(&procs, |p| p.open_files);
        assert_eq!(m.iter().map(|h| h.pid).collect::<Vec<_>>(), vec![2]);
        let even = [load(1, 25), load(2, 25), load(3, 25), load(4, 25)];
        assert_eq!(majority(&even, |p| p.open_files).len(), 2);
        assert!(majority(&[], |p| p.open_files).is_empty());
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration(42.0), "42s");
        assert_eq!(duration(600.0), "10m");
        assert_eq!(duration(7200.0), "2.0h");
        assert_eq!(duration(f64::INFINITY), "never");
    }
}
