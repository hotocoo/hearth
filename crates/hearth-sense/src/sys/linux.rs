//! Linux sensing through procfs, PSI and cgroup v2. Accelerators (NVML, AMD
//! sysfs) are read by later phases; until then none are reported.

use std::fs;

use crate::{
    Accelerator, HostMemory, LinuxMem, ProcessCounts, ProcessEntry, ProcessFootprint, RawHost,
    Result, SenseError, SystemLimits, TcpSocket, TcpState, now_ms,
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

/// The tightest cgroup v2 memory limit over this process: its own cgroup's
/// and every ancestor's, since a limit on a parent slice binds too. Returns
/// `(limit, usage, reclaimable)` of the binding level, where reclaimable is
/// that cgroup's `inactive_file` page cache, which the kernel takes back
/// before it fails an allocation. `max` (no limit) everywhere is `None`.
fn cgroup_memory() -> (Option<u64>, Option<u64>, Option<u64>) {
    let Ok(cgroup) = fs::read_to_string("/proc/self/cgroup") else {
        return (None, None, None);
    };
    let Some(rel) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) else {
        return (None, None, None);
    };
    cgroup_memory_at(std::path::Path::new("/sys/fs/cgroup"), rel)
}

/// [`cgroup_memory`] for the cgroup `rel` under the hierarchy mounted at
/// `root`.
fn cgroup_memory_at(root: &std::path::Path, rel: &str) -> (Option<u64>, Option<u64>, Option<u64>) {
    let mut best: Option<(u64, u64, u64)> = None;
    let mut path = root.join(rel.trim_start_matches('/').trim_end_matches('/'));
    loop {
        let num = |file: &str| {
            fs::read_to_string(path.join(file))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        };
        if let (Some(max), Some(current)) = (num("memory.max"), num("memory.current")) {
            let inactive = fs::read_to_string(path.join("memory.stat"))
                .ok()
                .and_then(|stat| {
                    stat.lines()
                        .find_map(|l| l.strip_prefix("inactive_file "))
                        .and_then(|v| v.trim().parse().ok())
                })
                .unwrap_or(0);
            let headroom = max
                .saturating_sub(current)
                .saturating_add(inactive)
                .min(max);
            if best.is_none_or(|(m, c, i)| headroom < m.saturating_sub(c).saturating_add(i).min(m))
            {
                best = Some((max, current, inactive));
            }
        }
        if path == root || !path.pop() {
            break;
        }
    }
    match best {
        Some((max, current, inactive)) => (Some(max), Some(current), Some(inactive)),
        None => (None, None, None),
    }
}

pub fn host_memory() -> Result<HostMemory> {
    let meminfo = read("/proc/meminfo", "read meminfo")?;
    let psi = fs::read_to_string("/proc/pressure/memory").unwrap_or_default();
    let (cgroup_max, cgroup_current, cgroup_reclaimable) = cgroup_memory();
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
    // Inside a memory-limited cgroup the limit is the machine as far as
    // this process is concerned: both totals and availability are the
    // smaller of the host's and the cgroup's.
    let (host_total, host_available) = (
        required(&meminfo, "MemTotal")?,
        required(&meminfo, "MemAvailable")?,
    );
    let (total, available) = match (cgroup_max, cgroup_current) {
        (Some(max), Some(current)) => (
            host_total.min(max),
            host_available.min(
                max.saturating_sub(current)
                    .saturating_add(cgroup_reclaimable.unwrap_or(0))
                    .min(max),
            ),
        ),
        _ => (host_total, host_available),
    };
    Ok(HostMemory {
        taken_at_ms: now_ms(),
        total_bytes: total,
        available_bytes: available,
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

/// Where each [`SystemLimits`] field comes from on this platform.
pub const LIMIT_SOURCES: &[(&str, &str)] = &[
    (
        "files_open",
        "/proc/sys/fs/file-nr: allocated minus allocated-but-unused",
    ),
    (
        "threads_total",
        "/proc/loadavg fourth field: total scheduling entities",
    ),
    ("files_max", "/proc/sys/fs/file-nr third field"),
    ("files_per_process_max", "/proc/sys/fs/nr_open"),
    ("threads_max", "/proc/sys/kernel/threads-max"),
    (
        "threads_per_process_max",
        "none: Linux bounds threads per user (RLIMIT_NPROC), not per process",
    ),
    ("processes_max", "/proc/sys/kernel/pid_max"),
];

pub fn system_limits() -> Result<SystemLimits> {
    // file-nr: allocated, allocated-but-unused, maximum.
    let file_nr: Vec<u64> = fs::read_to_string("/proc/sys/fs/file-nr")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|f| f.parse().ok())
        .collect();
    // loadavg's fourth field is "runnable/total" scheduling entities, i.e.
    // every thread on the machine.
    let threads_total = fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|l| l.split_whitespace().nth(3)?.split_once('/')?.1.parse().ok());
    Ok(SystemLimits {
        files_open: file_nr.first().zip(file_nr.get(1)).map(|(a, u)| a - u),
        threads_total,
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

/// Socket inodes a process holds, from its `fd/` symlinks (`socket:[123]`).
fn socket_inodes(pid: i32) -> Result<std::collections::HashSet<u64>> {
    let dir = fs::read_dir(format!("/proc/{pid}/fd")).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => SenseError::NoProcess(pid),
        _ => SenseError::NotPermitted(pid),
    })?;
    Ok(dir
        .filter_map(std::result::Result::ok)
        .filter_map(|e| fs::read_link(e.path()).ok())
        .filter_map(|target| {
            target
                .to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect())
}

/// `0100007F:1F90` (IPv4, little-endian words) or 32 hex digits (IPv6, four
/// little-endian words), as `/proc/net/tcp{,6}` print them.
fn parse_proc_addr(text: &str) -> Option<std::net::SocketAddr> {
    let (ip, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let words: Vec<u32> = (0..ip.len() / 8)
        .map(|i| u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok())
        .collect::<Option<_>>()?;
    let ip: std::net::IpAddr = match words.as_slice() {
        [w] => std::net::Ipv4Addr::from(w.swap_bytes()).into(),
        [a, b, c, d] => {
            let mut bytes = [0u8; 16];
            for (i, w) in [a, b, c, d].iter().enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
            std::net::Ipv6Addr::from(bytes).into()
        }
        _ => return None,
    };
    Some(std::net::SocketAddr::new(ip, port))
}

/// `TCP_*` values from the kernel's `include/net/tcp_states.h`.
fn linux_tcp_state(code: u8) -> TcpState {
    match code {
        1 => TcpState::Established,
        2 => TcpState::SynSent,
        3 => TcpState::SynReceived,
        4 => TcpState::FinWait1,
        5 => TcpState::FinWait2,
        6 => TcpState::TimeWait,
        7 => TcpState::Closed,
        8 => TcpState::CloseWait,
        9 => TcpState::LastAck,
        10 => TcpState::Listen,
        11 => TcpState::Closing,
        _ => TcpState::Unknown,
    }
}

fn parse_proc_net_tcp(text: &str) -> Vec<(u64, TcpSocket)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let local = parse_proc_addr(f.get(1)?)?;
            let remote = parse_proc_addr(f.get(2)?)?;
            let state = linux_tcp_state(u8::from_str_radix(f.get(3)?, 16).ok()?);
            let inode = f.get(9)?.parse().ok()?;
            Some((
                inode,
                TcpSocket {
                    local,
                    remote: (state != TcpState::Listen).then_some(remote),
                    state,
                },
            ))
        })
        .collect()
}

pub fn tcp_sockets(pid: i32) -> Result<Vec<TcpSocket>> {
    let inodes = socket_inodes(pid)?;
    // The process's own network namespace, which is what its sockets live in.
    let tables = ["tcp", "tcp6"]
        .iter()
        .filter_map(|t| fs::read_to_string(format!("/proc/{pid}/net/{t}")).ok());
    Ok(tables
        .flat_map(|t| parse_proc_net_tcp(&t))
        .filter(|(inode, _)| inodes.contains(inode))
        .map(|(_, socket)| socket)
        .collect())
}

/// The process group `pid` belongs to: the fifth field of `/proc/PID/stat`,
/// counted after the command name, which is in parentheses and may itself
/// contain spaces and parentheses.
pub fn process_group(pid: i32) -> Result<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| SenseError::Os {
        call: "read /proc/PID/stat",
        detail: e.to_string(),
    })?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(2))
        .and_then(|g| g.parse().ok())
        .ok_or_else(|| SenseError::Os {
            call: "parse /proc/PID/stat",
            detail: format!("no process group in {stat:?}"),
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tightest_cgroup_limit_over_the_process_binds() {
        let root = std::env::temp_dir().join(format!("hearth-cg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let leaf = root.join("slice/app");
        fs::create_dir_all(&leaf).unwrap();
        let w = |dir: &std::path::Path, max: &str, current: &str, inactive: u64| {
            fs::write(dir.join("memory.max"), max).unwrap();
            fs::write(dir.join("memory.current"), current).unwrap();
            fs::write(
                dir.join("memory.stat"),
                format!("anon 1\ninactive_file {inactive}\n"),
            )
            .unwrap();
        };
        w(&leaf, "max", "100", 0);
        w(&root.join("slice"), "1000", "900", 50);
        // The leaf has no limit; its parent slice does, with 150 left.
        assert_eq!(
            cgroup_memory_at(&root, "/slice/app"),
            (Some(1000), Some(900), Some(50))
        );
        assert_eq!(cgroup_memory_at(&root, "/nowhere"), (None, None, None));
        fs::remove_dir_all(root).unwrap();
    }

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
        assert!(limits.threads_total.unwrap() >= me.threads.unwrap());
    }

    #[test]
    fn proc_net_tcp_lines_parse() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1 0 100 0 0 10 0\n   1: 0100007F:C350 0100007F:1F90 08 00000000:00000000 00:00000000 00000000  1000        0 4343 1 0 100 0 0 10 0\n";
        let parsed = parse_proc_net_tcp(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, 4242);
        assert_eq!(parsed[0].1.local, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(parsed[0].1.state, TcpState::Listen);
        assert!(parsed[0].1.remote.is_none());
        assert_eq!(parsed[1].1.state, TcpState::CloseWait);
        assert_eq!(parsed[1].1.remote, Some("127.0.0.1:8080".parse().unwrap()));
        let v6 = parse_proc_addr("00000000000000000000000001000000:0050").unwrap();
        assert_eq!(v6, "[::1]:80".parse().unwrap());
    }

    #[test]
    fn live_listening_socket_is_seen() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let sockets = tcp_sockets(std::process::id().cast_signed()).unwrap();
        assert!(
            sockets
                .iter()
                .any(|s| s.state == TcpState::Listen && s.local.port() == port)
        );
    }

    #[test]
    fn live_own_footprint_is_readable() {
        let fp = process_footprint(std::process::id().cast_signed()).unwrap();
        assert!(fp.footprint_bytes > 0);
    }
}
