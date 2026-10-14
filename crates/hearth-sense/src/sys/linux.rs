//! Linux sensing through procfs, PSI and cgroup v2. Accelerators (NVML, AMD
//! sysfs) are read by later phases; until then none are reported.

use std::fs;

use crate::{
    Accelerator, HostMemory, LinuxMem, ProcessCounts, ProcessEntry, ProcessFootprint, RawHost,
    Result, SenseError, SystemLimits, now_ms,
};

/// Parses `Key:   123 kB` lines into bytes.
fn kb_field(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?.strip_prefix(':')?;
        let mut parts = rest.split_whitespace();
        let value: u64 = parts.next()?.parse().ok()?;
        match parts.next() {
            Some("kB") => Some(value * 1024),
            None => Some(value),
            Some(_) => None,
        }
    })
}

fn required(text: &str, key: &'static str) -> Result<u64> {
    kb_field(text, key).ok_or(SenseError::Parse {
        what: "meminfo",
        detail: format!("missing {key}"),
    })
}

fn read(path: &str, call: &'static str) -> Result<String> {
    fs::read_to_string(path).map_err(|e| SenseError::Os {
        call,
        detail: format!("{path}: {e}"),
    })
}

/// `some avg10=0.12 avg60=...` and `full avg10=...` from PSI.
fn psi_avg10(text: &str, kind: &str) -> Option<f64> {
    text.lines()
        .find(|l| l.starts_with(kind))?
        .split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))?
        .parse()
        .ok()
}

/// The cgroup v2 limit and usage of this process's own cgroup. `max` means no
/// limit and is reported as `None`.
fn cgroup_memory() -> (Option<u64>, Option<u64>) {
    let Ok(cgroup) = fs::read_to_string("/proc/self/cgroup") else {
        return (None, None);
    };
    let Some(rel) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) else {
        return (None, None);
    };
    let base = format!("/sys/fs/cgroup{}", rel.trim_end_matches('/'));
    let num = |file: &str| {
        fs::read_to_string(format!("{base}/{file}"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    (num("memory.max"), num("memory.current"))
}

pub fn host_memory() -> Result<HostMemory> {
    let meminfo = read("/proc/meminfo", "read meminfo")?;
    let psi = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    let (cgroup_max, cgroup_current) = cgroup_memory();
    let swap_total = required(&meminfo, "SwapTotal")?;
    let raw = LinuxMem {
        mem_free: required(&meminfo, "MemFree")?,
        buffers: required(&meminfo, "Buffers")?,
        cached: required(&meminfo, "Cached")?,
        psi_some_avg10: psi_avg10(&psi, "some"),
        psi_full_avg10: psi_avg10(&psi, "full"),
        cgroup_max_bytes: cgroup_max,
        cgroup_current_bytes: cgroup_current,
    };
    Ok(HostMemory {
        taken_at_ms: now_ms(),
        total_bytes: required(&meminfo, "MemTotal")?,
        available_bytes: required(&meminfo, "MemAvailable")?,
        swap_total_bytes: swap_total,
        swap_used_bytes: swap_total.saturating_sub(required(&meminfo, "SwapFree")?),
        // Linux publishes stall percentages, not levels; see `raw`.
        pressure: None,
        raw: RawHost::Linux(raw),
    })
}

pub fn process_footprint(pid: i32) -> Result<ProcessFootprint> {
    let rollup = match fs::read_to_string(format!("/proc/{pid}/smaps_rollup")) {
        Ok(text) => text,
        Err(e) => {
            return Err(match e.kind() {
                std::io::ErrorKind::NotFound => SenseError::NoProcess(pid),
                std::io::ErrorKind::PermissionDenied => SenseError::NotPermitted(pid),
                _ => SenseError::Os {
                    call: "read smaps_rollup",
                    detail: e.to_string(),
                },
            });
        }
    };
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let missing = |key: &'static str| SenseError::Parse {
        what: "smaps_rollup",
        detail: format!("missing {key}"),
    };
    Ok(ProcessFootprint {
        pid,
        footprint_bytes: kb_field(&rollup, "Pss").ok_or_else(|| missing("Pss"))?,
        resident_bytes: kb_field(&rollup, "Rss").ok_or_else(|| missing("Rss"))?,
        wired_bytes: None,
        peak_bytes: kb_field(&status, "VmHWM"),
    })
}

pub fn list_processes() -> Result<Vec<ProcessEntry>> {
    let dir = fs::read_dir("/proc").map_err(|e| SenseError::Os {
        call: "read_dir /proc",
        detail: e.to_string(),
    })?;
    Ok(dir
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            let name = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            // `exe` is unreadable for other users' processes and gains a
            // " (deleted)" suffix once the binary is replaced on disk.
            let path = fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned());
            Some(ProcessEntry {
                pid,
                name: name.trim_end().to_string(),
                path,
            })
        })
        .collect())
}

pub fn accelerators() -> Result<Vec<Accelerator>> {
    Ok(Vec::new())
}

fn read_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn system_limits() -> Result<SystemLimits> {
    // file-nr: allocated, allocated-but-unused, maximum.
    let file_nr: Vec<u64> = fs::read_to_string("/proc/sys/fs/file-nr")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .collect();
    Ok(SystemLimits {
        files_open: file_nr.first().zip(file_nr.get(1)).map(|(a, u)| a - u),
        files_max: file_nr.get(2).copied(),
        files_per_process_max: read_u64("/proc/sys/fs/nr_open"),
        threads_max: read_u64("/proc/sys/kernel/threads-max"),
        // Linux bounds threads per process only through threads-max and
        // RLIMIT_NPROC, which is per user, not per process.
        threads_per_process_max: None,
        processes_max: read_u64("/proc/sys/kernel/pid_max"),
    })
}

pub fn process_counts(pid: i32) -> Result<ProcessCounts> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SenseError::NoProcess(pid),
        _ => SenseError::Os {
            call: "read status",
            detail: e.to_string(),
        },
    })?;
    Ok(ProcessCounts {
        pid,
        open_files: fs::read_dir(format!("/proc/{pid}/fd"))
            .ok()
            .map(|d| d.count() as u64),
        threads: kb_field(&status, "Threads"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kb_fields_convert_to_bytes() {
        let text =
            "MemTotal:       16303428 kB\nMemAvailable:    9000000 kB\nHugePages_Total:       0\n";
        assert_eq!(kb_field(text, "MemTotal"), Some(16_303_428 * 1024));
        assert_eq!(kb_field(text, "HugePages_Total"), Some(0));
        assert_eq!(kb_field(text, "Mem"), None);
    }

    #[test]
    fn psi_avg10_reads_both_lines() {
        let text = "some avg10=1.50 avg60=0.10 avg300=0.00 total=123\nfull avg10=0.25 avg60=0.00 avg300=0.00 total=9\n";
        assert_eq!(psi_avg10(text, "some"), Some(1.5));
        assert_eq!(psi_avg10(text, "full"), Some(0.25));
    }

    #[test]
    fn live_host_reading_is_plausible() {
        let host = host_memory().unwrap();
        assert!(host.available_bytes <= host.total_bytes);
    }

    #[test]
    fn live_own_counts_and_limits() {
        let me = process_counts(std::process::id().cast_signed()).unwrap();
        assert!(me.open_files.unwrap() >= 3);
        assert!(me.threads.unwrap() >= 1);
        let limits = system_limits().unwrap();
        assert!(limits.files_open.unwrap() <= limits.files_max.unwrap());
    }

    #[test]
    fn live_own_footprint_is_readable() {
        let fp = process_footprint(std::process::id().cast_signed()).unwrap();
        assert!(fp.footprint_bytes > 0);
    }
}
