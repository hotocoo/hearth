//! `hearth incidents`: what the OS recorded about crashes and kills.

use std::fmt::Write as _;
use std::path::PathBuf;

use hearth_sense::incidents::{self, Incident, Kind, PanicClass};

#[derive(clap::Args)]
pub struct Args {
    /// Report directory to scan instead of the OS defaults (repeatable).
    #[arg(long = "dir", value_name = "DIR")]
    dirs: Vec<PathBuf>,
    /// Only incidents whose recorded time sorts at or after this prefix,
    /// e.g. 2026-10 or 2026-10-12.
    #[arg(long, value_name = "TIME")]
    since: Option<String>,
    /// Also list resource-limit reports (disk writes, CPU, wakeups), which
    /// are counted but not listed by default.
    #[arg(long)]
    all: bool,
}

/// Words journalctl prints when built without PCRE and so refusing `--grep`.
const NO_GREP_HINT: &str = "pattern";

/// Exit status journalctl uses for "no entries matched".
const NO_MATCH_EXIT: i32 = 1;

/// `journalctl` arguments for the kernel log: every boot (`-k` would imply
/// the current one only), narrowed by the user's `--since` and by the
/// patterns the parser reads, so the whole journal is not buffered.
fn journal_args(since: Option<i64>, grep: bool) -> Vec<String> {
    let mut args: Vec<String> = [
        "_TRANSPORT=kernel",
        "-o",
        "short-iso",
        "--no-pager",
        "--quiet",
    ]
    .map(String::from)
    .into();
    if let Some(epoch) = since {
        args.push(format!("--since=@{epoch}"));
    }
    if grep {
        args.push(format!("--grep={}", incidents::kernel_log_pattern()));
    }
    args
}

/// Whether a failed `journalctl --grep` was only the build lacking pattern
/// support, which a retry without `--grep` fixes.
fn grep_unsupported(stderr: &str) -> bool {
    stderr.to_lowercase().contains(NO_GREP_HINT)
}

/// The kernel log, where Linux records OOM kills and hung tasks, from every
/// boot the journal kept: `journalctl -k` would mean the current boot only,
/// and the kill that matters is usually the one before the reboot. Reading
/// it needs membership in `systemd-journal` or `adm` on most systems.
fn kernel_log(since: Option<i64>) -> Result<Vec<Incident>, String> {
    let run = |grep: bool| {
        std::process::Command::new("journalctl")
            .args(journal_args(since, grep))
            .output()
            .map_err(|e| format!("journalctl: {e}"))
    };
    let mut out = run(true)?;
    if !out.status.success() && grep_unsupported(&String::from_utf8_lossy(&out.stderr)) {
        out = run(false)?;
    }
    // Newer journalctl exits 1, silently, when --grep matched nothing.
    let nothing_matched = out.status.code() == Some(NO_MATCH_EXIT) && out.stderr.is_empty();
    if !out.status.success() && !nothing_matched {
        return Err(format!(
            "journalctl _TRANSPORT=kernel: {}; reading the kernel log usually needs the systemd-journal or adm group",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(incidents::from_kernel_log(
        &String::from_utf8_lossy(&out.stdout),
        std::path::Path::new("journalctl _TRANSPORT=kernel"),
    ))
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let dirs = if args.dirs.is_empty() {
        incidents::default_dirs()
    } else {
        args.dirs.clone()
    };
    // Compared as instants: the formats differ in separator and offset.
    let since = match args.since.as_deref() {
        Some(s) => Some(
            incidents::epoch_secs(s).ok_or_else(|| format!("--since {s}: not a date or time"))?,
        ),
        None => None,
    };
    let mut found = incidents::scan(&dirs);
    let use_kernel_log = cfg!(target_os = "linux") && args.dirs.is_empty();
    if use_kernel_log {
        found.extend(kernel_log(since)?);
    } else if dirs.is_empty() {
        return Err("no report directories known for this OS; pass --dir".into());
    }
    let found: Vec<Incident> = found
        .into_iter()
        .filter(|i| {
            since.is_none_or(|s| {
                i.time
                    .as_deref()
                    .and_then(incidents::epoch_secs)
                    .is_some_and(|t| t >= s)
            })
        })
        .collect();
    if json {
        return crate::to_json(&found);
    }
    let mut out = String::new();
    if found.is_empty() {
        let _ = writeln!(
            out,
            "no incidents in {} report directories{}",
            dirs.len(),
            if use_kernel_log {
                " or the kernel log"
            } else {
                ""
            }
        );
        return Ok(out);
    }
    let limits = found
        .iter()
        .filter(|i| matches!(i.kind, Kind::ResourceLimit { .. }))
        .count();
    for i in found
        .iter()
        .filter(|i| args.all || !matches!(i.kind, Kind::ResourceLimit { .. }))
    {
        let when = i.time.as_deref().unwrap_or("unknown time");
        let line = describe(&i.kind);
        let _ = writeln!(out, "{when}  {line}");
    }
    if !args.all && limits > 0 {
        let _ = writeln!(
            out,
            "{limits} resource-limit reports not listed (--all shows them)"
        );
    }
    Ok(out)
}

fn describe(kind: &Kind) -> String {
    match kind {
        Kind::KernelPanic { class, message } => {
            let class = match class {
                PanicClass::Watchdog => {
                    "watchdog (machine stopped scheduling; not memory exhaustion)"
                }
                PanicClass::GpuDriver => "GPU memory driver",
                PanicClass::Other => "other",
            };
            format!("KERNEL PANIC [{class}] {message}")
        }
        Kind::Jetsam {
            largest_process,
            killed,
        } => format!(
            "JETSAM largest={} killed={}",
            largest_process.as_deref().unwrap_or("?"),
            killed
                .iter()
                .map(|k| format!("{} ({})", k.name, k.reason))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Kind::ResourceLimit {
            command,
            pid,
            event,
            action,
            detail,
        } => format!(
            "resource limit: {} (pid {}) {} · action {} · {}",
            command.as_deref().unwrap_or("?"),
            pid.map_or("?".into(), |p| p.to_string()),
            event.as_deref().unwrap_or("?"),
            action.as_deref().unwrap_or("?"),
            detail.as_deref().unwrap_or("")
        ),
        Kind::ShutdownStall => "shutdown stalled on a process".into(),
        Kind::OomKill {
            process,
            pid,
            anon_rss_bytes,
            cgroup,
        } => format!(
            "OOM KILL{} {process} (pid {pid}){}",
            if *cgroup { " [cgroup limit]" } else { "" },
            anon_rss_bytes.map_or(String::new(), |b| format!(
                ", freed {} of anonymous memory",
                crate::units::bytes(b)
            ))
        ),
        Kind::HungTask {
            process,
            pid,
            seconds,
        } => {
            format!("HUNG TASK {process} (pid {pid}) stuck in the kernel for more than {seconds} s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_args_narrow_by_time_and_pattern_and_retry_is_detected() {
        let args = journal_args(Some(1_700_000_000), true);
        assert!(args.contains(&"--since=@1700000000".to_string()));
        assert!(args.iter().any(|a| a.starts_with("--grep=")));
        assert!(
            !journal_args(None, false)
                .iter()
                .any(|a| a.starts_with("--since") || a.starts_with("--grep"))
        );
        assert!(grep_unsupported(
            "Compiled without pattern matching support"
        ));
        assert!(!grep_unsupported("Permission denied"));
    }
}
