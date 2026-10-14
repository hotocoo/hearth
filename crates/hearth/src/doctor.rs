//! `hearth doctor`: the resources whose exhaustion takes a whole machine down.
//!
//! No thresholds are invented here. A finding is either something the kernel
//! itself declares (a memory pressure level) or a projection from measured
//! growth (`--watch`), stated with the time to the kernel's own limit.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use hearth_sense::{BoxHealth, PressureLevel, ProcessLoad};
use serde::Serialize;

use crate::units::{bytes, percent};

#[derive(clap::Args)]
pub struct Args {
    /// Sample twice, this many seconds apart, and project when each growing
    /// resource would hit its limit at the measured rate.
    #[arg(long, value_name = "SECONDS")]
    watch: Option<u64>,
}

#[derive(Serialize)]
struct Report {
    health: BoxHealth,
    /// Fewest processes that together hold at least half of all open files.
    file_majority: Vec<Holder>,
    /// Fewest processes that together hold at least half of all threads.
    thread_majority: Vec<Holder>,
    /// Process with the largest share of its own open-file limit.
    nearest_file_limit: Option<Holder>,
    trend: Option<Trend>,
    findings: Vec<Finding>,
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
    message: String,
    /// What to do about it, for a person or an agent.
    remediation: String,
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let first = hearth_sense::box_health().map_err(|e| e.to_string())?;
    let (health, trend) = match args.watch {
        Some(secs) => {
            let started = Instant::now();
            std::thread::sleep(Duration::from_secs(secs));
            let second = hearth_sense::box_health().map_err(|e| e.to_string())?;
            let elapsed = started.elapsed().as_secs_f64();
            let trend = trend(&first, &second, elapsed);
            (second, Some(trend))
        }
        None => (first, None),
    };
    let report = build(health, trend);
    if json {
        crate::to_json(&report)
    } else {
        Ok(render(&report))
    }
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

#[allow(clippy::cast_precision_loss)]
fn build(health: BoxHealth, trend: Option<Trend>) -> Report {
    let file_majority = majority(&health.processes, |p| p.open_files);
    let thread_majority = majority(&health.processes, |p| p.threads);
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
    let mut findings = Vec::new();
    match health.memory.pressure {
        Some(PressureLevel::Warn) => findings.push(Finding {
            severity: "warn",
            message: "the kernel reports memory pressure: warn".into(),
            remediation: "do not load more models; unload idle ones or reduce context sizes".into(),
        }),
        Some(PressureLevel::Critical) => findings.push(Finding {
            severity: "critical",
            message: "the kernel reports memory pressure: critical".into(),
            remediation:
                "free memory now: stop the largest idle model server; jetsam kills are imminent"
                    .into(),
        }),
        _ => {}
    }
    if let Some(t) = &trend {
        if let (Some(rate), Some(open), Some(max)) = (
            t.files_per_second,
            health.limits.files_open,
            health.limits.files_max,
        ) && rate > 0.0
        {
            findings.push(Finding {
                severity: "warn",
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
                    message: format!(
                        "{} (pid {}) {} growing {:.1}/s; its limit in {}",
                        g.name,
                        g.pid,
                        g.resource,
                        g.per_second,
                        duration(secs)
                    ),
                    remediation: format!(
                        "if this continues, restart {} or fix the leak (for sockets: close on client disconnect)",
                        g.name
                    ),
                });
            }
        }
    }
    Report {
        health,
        file_majority,
        thread_majority,
        nearest_file_limit,
        trend,
        findings,
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
