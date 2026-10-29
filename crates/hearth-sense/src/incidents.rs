//! Crash forensics: what took the machine down, read from the reports the OS
//! already writes. Classification uses only text the kernel puts in those
//! reports; nothing is inferred from timing or guessed.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct Incident {
    /// As written in the report (local time with offset when present).
    pub time: Option<String>,
    pub source: PathBuf,
    #[serde(flatten)]
    pub kind: Kind,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    /// The kernel panicked and the machine rebooted.
    KernelPanic { class: PanicClass, message: String },
    /// The kernel killed processes to reclaim memory.
    Jetsam {
        largest_process: Option<String>,
        killed: Vec<Killed>,
    },
    /// A process exceeded a resource limit the OS monitors (disk writes, CPU,
    /// wakeups); the report says whether any action was taken.
    ResourceLimit {
        command: Option<String>,
        pid: Option<i64>,
        event: Option<String>,
        action: Option<String>,
        detail: Option<String>,
    },
    /// Shutdown waited on a process that would not exit.
    ShutdownStall,
    /// The Linux OOM killer killed a process, machine-wide or inside a
    /// memory cgroup.
    OomKill {
        process: String,
        pid: i64,
        /// Anonymous memory it held when killed: what the kill freed.
        anon_rss_bytes: Option<u64>,
        /// Killed by its cgroup's limit, not by the machine running out.
        cgroup: bool,
    },
    /// A task sat in uninterruptible sleep past the kernel's hung-task
    /// timeout: what a process stuck on a dead device or driver looks like.
    HungTask {
        process: String,
        pid: i64,
        seconds: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PanicClass {
    /// The watchdog rebooted a machine that stopped scheduling (thread or CPU
    /// starvation). Not a memory exhaustion.
    Watchdog,
    /// The panic originated in the GPU memory driver.
    GpuDriver,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct Killed {
    pub name: String,
    pub reason: String,
}

/// Where this OS writes crash and resource reports.
#[must_use]
pub fn default_dirs() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        let mut dirs = vec![PathBuf::from("/Library/Logs/DiagnosticReports")];
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join("Library/Logs/DiagnosticReports"));
        }
        let retired: Vec<PathBuf> = dirs.iter().map(|d| d.join("Retired")).collect();
        dirs.extend(retired);
        dirs
    } else {
        Vec::new()
    }
}

/// Every incident found in `dirs`, oldest first. Unreadable files are skipped:
/// most report directories mix files only root can read.
#[must_use]
pub fn scan(dirs: &[PathBuf]) -> Vec<Incident> {
    let mut found: Vec<Incident> = dirs
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flatten()
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| parse_file(&entry.path()))
        .collect();
    found.sort_by(|a, b| a.time.cmp(&b.time));
    found
}

#[must_use]
pub fn parse_file(path: &Path) -> Option<Incident> {
    let name = path.file_name()?.to_str()?;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let kind_by_name = match ext.as_str() {
        "panic" => "panic",
        "ips" if name.starts_with("JetsamEvent") => "jetsam",
        "diag" => "diag",
        "shutdownstall" => "stall",
        _ => return None,
    };
    let text = fs::read_to_string(path).ok()?;
    let kind = match kind_by_name {
        "panic" => parse_panic(&text)?,
        "jetsam" => parse_jetsam(&text)?,
        "diag" => parse_diag(&text)?,
        _ => Kind::ShutdownStall,
    };
    let header = json_header(&text);
    let time = header_field(&text, "Date/Time")
        .map(str::to_string)
        .or_else(|| {
            header
                .as_ref()
                .and_then(|h| h["timestamp"].as_str().map(str::to_string))
        })
        .or_else(|| time_from_name(name))
        // Panic bundle metadata names the full report, whose name is dated.
        .or_else(|| {
            header
                .as_ref()
                .and_then(|h| h["log_path"].as_str())
                .and_then(|p| Path::new(p).file_name()?.to_str())
                .and_then(time_from_name)
        });
    Some(Incident {
        time,
        source: path.to_path_buf(),
        kind,
    })
}

/// Incidents in kernel log lines as `journalctl -o short-iso` prints
/// them: `<time> <host> kernel: <message>`. The messages matched are the
/// kernel's own format strings:
/// * `mm/oom_kill.c`: `"%s: Killed process %d (%s) total-vm:%lukB,
///   anon-rss:%lukB, ..."`, where `%s` is `Out of memory` or `Memory cgroup
///   out of memory`;
/// * `kernel/hung_task.c`: `"INFO: task %s:%d blocked for more than %ld
///   seconds."`.
#[must_use]
pub fn from_kernel_log(text: &str, source: &Path) -> Vec<Incident> {
    text.lines()
        .filter_map(|line| {
            let (time, message) = match line.split_once(" kernel: ") {
                Some((head, message)) => (head.split_whitespace().next(), message),
                None => (None, line),
            };
            let kind = oom_kill(message).or_else(|| hung_task(message))?;
            Some(Incident {
                time: time.map(str::to_string),
                source: source.to_path_buf(),
                kind,
            })
        })
        .collect()
}

/// Text the kernel's OOM-kill line carries after its cause. Shared with
/// [`kernel_log_pattern`] so the journal filter cannot drift from the parser.
const OOM_MARKER: &str = ": Killed process ";
/// The two fixed ends of the kernel's hung-task line, around `%s:%d`.
const HUNG_START: &str = "INFO: task ";
const HUNG_MID: &str = " blocked for more than ";

/// A regex (for `journalctl --grep`) matching exactly the lines
/// [`from_kernel_log`] can turn into incidents, so the journal need not be
/// read in full. Built from the parser's own markers.
#[must_use]
pub fn kernel_log_pattern() -> String {
    format!("{OOM_MARKER}|{HUNG_START}.*{HUNG_MID}")
}

fn oom_kill(message: &str) -> Option<Kind> {
    let (cause, rest) = message.split_once(OOM_MARKER)?;
    let cgroup = cause.ends_with("Memory cgroup out of memory");
    if !cgroup && !cause.ends_with("Out of memory") {
        return None;
    }
    let (pid, rest) = rest.split_once(" (")?;
    let (process, rest) = rest.split_once(") ")?;
    let anon_rss_bytes = rest
        .split(", ")
        .find_map(|f| {
            f.strip_prefix("anon-rss:")?
                .strip_suffix("kB")?
                .parse::<u64>()
                .ok()
        })
        .map(|kb| kb.saturating_mul(1024));
    Some(Kind::OomKill {
        process: process.to_string(),
        pid: pid.trim().parse().ok()?,
        anon_rss_bytes,
        cgroup,
    })
}

fn hung_task(message: &str) -> Option<Kind> {
    let rest = message.split_once(HUNG_START)?.1;
    let (task, rest) = rest.split_once(HUNG_MID)?;
    let (process, pid) = task.rsplit_once(':')?;
    let seconds = rest.split_whitespace().next()?.parse().ok()?;
    Some(Kind::HungTask {
        process: process.to_string(),
        pid: pid.parse().ok()?,
        seconds,
    })
}

/// `name_2026-10-12-091733_host.diag` or `panic-full-2026-09-22-085938.0002.panic`
/// → `2026-10-12 09:17:33`.
fn time_from_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    (0..bytes.len().saturating_sub(16)).find_map(|i| {
        let s = name.get(i..i + 17)?;
        let b = s.as_bytes();
        let shape = b.iter().enumerate().all(|(j, c)| match j {
            4 | 7 | 10 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
        shape.then(|| format!("{} {}:{}:{}", &s[..10], &s[11..13], &s[13..15], &s[15..17]))
    })
}

/// Seconds since the Unix epoch of a timestamp in any form incidents carry:
/// `2026-10-14T03:12:09+0800`, `2026-10-12 09:17:33.00 +0800`,
/// `2026-10-12 09:17:33`, or a bare date. A missing offset reads as UTC (the
/// reports do not say which zone they are in). Lets `--since` compare times
/// across formats, which as strings differ in separator and offset.
#[must_use]
pub fn epoch_secs(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', ' ']).unwrap_or((text, ""));
    let mut ymd = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (ymd.next()??, ymd.next()??, ymd.next()??);
    let rest = rest.trim();
    let (clock, zone) = match rest.find(['+', '-', 'Z', ' ']) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    // A bare date has no clock: midnight.
    let mut parts = clock.split(':').filter(|p| !p.is_empty());
    let mut field = || {
        parts
            .next()
            .map_or(Some(0), |p| p.split('.').next()?.parse::<i64>().ok())
    };
    let (hour, min, sec) = (field()?, field()?, field()?);
    let offset = match zone.as_bytes().first() {
        Some(&sign @ (b'+' | b'-')) => {
            let digits: String = zone[1..].chars().filter(char::is_ascii_digit).collect();
            let (oh, om) = (
                digits.get(..2)?.parse::<i64>().ok()?,
                digits.get(2..4)?.parse::<i64>().ok()?,
            );
            let secs = oh * SECS_PER_HOUR + om * SECS_PER_MINUTE;
            if sign == b'-' { -secs } else { secs }
        }
        _ => 0,
    };
    // Days from civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * SECS_PER_DAY + hour * SECS_PER_HOUR + min * SECS_PER_MINUTE + sec - offset)
}

const SECS_PER_MINUTE: i64 = 60;
const SECS_PER_HOUR: i64 = 3600;
const SECS_PER_DAY: i64 = 86_400;

/// `Key:   value` lines of the text report format.
fn header_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix(':')
            .map(str::trim)
            .filter(|v| !v.is_empty())
    })
}

/// `.ips` and `.panic` files start with a one-line JSON header.
fn json_header(text: &str) -> Option<Value> {
    serde_json::from_str(text.lines().next()?).ok()
}

fn parse_panic(text: &str) -> Option<Kind> {
    let header = json_header(text);
    // The bundle metadata (`.contents.panic`) and full reports both carry the
    // panic string; full reports may also carry it in a JSON body.
    let message = header
        .as_ref()
        .and_then(|h| h["panic_string"].as_str().map(str::to_string))
        .or_else(|| {
            let body: Value = serde_json::from_str(text.split_once('\n')?.1).ok()?;
            body["panicString"].as_str().map(str::to_string)
        })
        .or_else(|| {
            text.lines()
                .find(|l| l.starts_with("panic("))
                .map(str::to_string)
        })?;
    let first = message.lines().next().unwrap_or_default().to_string();
    let lower = message.to_ascii_lowercase();
    let class = if lower.contains("watchdog") {
        PanicClass::Watchdog
    } else if lower.contains("iogpu") || lower.contains("agx") {
        PanicClass::GpuDriver
    } else {
        PanicClass::Other
    };
    Some(Kind::KernelPanic {
        class,
        message: first,
    })
}

fn parse_jetsam(text: &str) -> Option<Kind> {
    let (_, body) = text.split_once('\n')?;
    let body: Value = serde_json::from_str(body).ok()?;
    let killed = body["processes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Killed {
                name: p["name"].as_str()?.to_string(),
                reason: p["reason"].as_str()?.to_string(),
            })
        })
        .collect();
    Some(Kind::Jetsam {
        largest_process: body["largestProcess"].as_str().map(str::to_string),
        killed,
    })
}

fn parse_diag(text: &str) -> Option<Kind> {
    let field = |k| header_field(text, k).map(str::to_string);
    let event = field("Event")?;
    let detail = ["Writes", "CPU", "Wakeups"].iter().find_map(|k| field(k));
    Some(Kind::ResourceLimit {
        command: field("Command"),
        pid: header_field(text, "PID").and_then(|p| p.parse().ok()),
        event: Some(event),
        action: field("Action taken"),
        detail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hearth-incidents-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn epoch_secs_compares_across_formats() {
        let kernel = epoch_secs("2026-10-14T03:12:09+0800").unwrap();
        let report = epoch_secs("2026-10-12 09:17:33.00 +0800").unwrap();
        assert!(kernel > report);
        // Same instant, different zone and separator.
        assert_eq!(
            epoch_secs("2026-10-12T01:17:33+0000"),
            epoch_secs("2026-10-12 09:17:33.00 +0800")
        );
        assert_eq!(epoch_secs("1970-01-02"), Some(86_400));
        assert_eq!(epoch_secs("garbage"), None);
    }

    #[test]
    fn classifies_panic_metadata() {
        let dir = tmp();
        let gpu = write(
            &dir,
            ".contents.panic",
            r#"{"log_path":"/x/panic-full-2026-09-22-085938.0002.panic","panic_string":"panic(cpu 5 caller 0x1): \"completeMemory() prepare count underflow\" @IOGPUMemory.cpp:550\nDebugger message: panic"}"#,
        );
        let inc = parse_file(&gpu).unwrap();
        assert_eq!(inc.time.as_deref(), Some("2026-09-22 08:59:38"));
        match inc.kind {
            Kind::KernelPanic { class, message } => {
                assert_eq!(class, PanicClass::GpuDriver);
                assert!(message.contains("IOGPUMemory.cpp"));
                assert!(!message.contains("Debugger"));
            }
            other => panic!("{other:?}"),
        }
        let dog = write(
            &dir,
            "panic-full-2026-07-10-092100.0001.panic",
            "{\"bug_type\":\"210\"}\n{\"panicString\":\"panic(cpu 0 caller 0x2): watchdog timeout: no checkins from watchdogd in 92 seconds\"}",
        );
        let inc = parse_file(&dog).unwrap();
        assert!(matches!(
            inc.kind,
            Kind::KernelPanic {
                class: PanicClass::Watchdog,
                ..
            }
        ));
        assert_eq!(inc.time.as_deref(), Some("2026-07-10 09:21:00"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_resource_reports_and_jetsam() {
        let dir = tmp();
        let diag = write(
            &dir,
            "llama_2026-10-12-091733_host.diag",
            "Date/Time:        2026-10-12 08:48:17.217 +0800\nCommand:          llama\nPID:              6608\n\nEvent:            disk writes\nAction taken:     none\nWrites:           2147.50 MB of file backed memory dirtied over 1754 seconds\n",
        );
        match parse_file(&diag).unwrap() {
            Incident {
                time,
                kind:
                    Kind::ResourceLimit {
                        command,
                        pid,
                        event,
                        action,
                        detail,
                    },
                ..
            } => {
                assert_eq!(time.as_deref(), Some("2026-10-12 08:48:17.217 +0800"));
                assert_eq!(command.as_deref(), Some("llama"));
                assert_eq!(pid, Some(6608));
                assert_eq!(event.as_deref(), Some("disk writes"));
                assert_eq!(action.as_deref(), Some("none"));
                assert!(detail.unwrap().starts_with("2147.50 MB"));
            }
            other => panic!("{other:?}"),
        }
        let jetsam = write(
            &dir,
            "JetsamEvent-2026-07-10-085700.ips",
            "{\"bug_type\":\"298\",\"timestamp\":\"2026-07-10 08:57:00.00 +0800\"}\n{\"largestProcess\":\"llama-server\",\"processes\":[{\"name\":\"knowledgeconstructiond\",\"reason\":\"per-process-limit\"},{\"name\":\"idle\"}]}",
        );
        match parse_file(&jetsam).unwrap().kind {
            Kind::Jetsam {
                largest_process,
                killed,
            } => {
                assert_eq!(largest_process.as_deref(), Some("llama-server"));
                assert_eq!(killed.len(), 1);
                assert_eq!(killed[0].reason, "per-process-limit");
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_file(&write(&dir, "notes.txt", "x")).is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_oom_kills_and_hung_tasks_from_the_kernel_log() {
        let log = "\
2026-10-14T03:12:09+0800 box kernel: llama-server invoked oom-killer: gfp_mask=0x140cca(GFP_HIGHUSER_MOVABLE|__GFP_COMP), order=0, oom_score_adj=0
2026-10-14T03:12:09+0800 box kernel: Out of memory: Killed process 4242 (llama-server) total-vm:61234560kB, anon-rss:41943040kB, file-rss:1024kB, shmem-rss:0kB, UID:1000 pgtables:90000kB oom_score_adj:0
2026-10-14T04:00:00+0800 box kernel: Memory cgroup out of memory: Killed process 77 (python3) total-vm:100kB, anon-rss:2048kB, file-rss:0kB, shmem-rss:0kB, UID:1000 pgtables:8kB oom_score_adj:0
2026-10-14T05:00:00+0800 box kernel: INFO: task ollama:991 blocked for more than 122 seconds.
2026-10-14T05:00:01+0800 box kernel: usb 1-1: new high-speed USB device number 2
";
        let found = from_kernel_log(log, Path::new("journalctl -k"));
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].time.as_deref(), Some("2026-10-14T03:12:09+0800"));
        match &found[0].kind {
            Kind::OomKill {
                process,
                pid,
                anon_rss_bytes,
                cgroup,
            } => {
                assert_eq!(
                    (process.as_str(), *pid, *cgroup),
                    ("llama-server", 4242, false)
                );
                assert_eq!(*anon_rss_bytes, Some(41_943_040 * 1024));
            }
            k => panic!("{k:?}"),
        }
        assert!(matches!(found[1].kind, Kind::OomKill { cgroup: true, .. }));
        assert!(matches!(
            &found[2].kind,
            Kind::HungTask { process, pid: 991, seconds: 122 } if process == "ollama"
        ));
    }

    #[test]
    fn kernel_log_pattern_matches_what_the_parser_reads() {
        let re = kernel_log_pattern();
        assert!(
            re.contains(": Killed process ") && re.contains("INFO: task .* blocked for more than ")
        );
    }

    #[test]
    fn time_from_report_names() {
        assert_eq!(
            time_from_name("node_2026-10-08-101901_host.diag").as_deref(),
            Some("2026-10-08 10:19:01")
        );
        assert_eq!(time_from_name("nothing.diag"), None);
    }
}
