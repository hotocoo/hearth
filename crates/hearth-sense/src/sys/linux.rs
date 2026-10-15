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

/// The cgroup v2 memory limits over this process: its own cgroup's and
/// every ancestor's, since a limit on a parent slice binds too.
#[derive(Debug, Default, PartialEq, Eq)]
struct Cgroup {
    /// The smallest `memory.max` of any level: the most this process's
    /// cgroup tree can ever hold.
    max: Option<u64>,
    /// Usage of the level with the least room left.
    current: Option<u64>,
    /// Room left at that level: limit - usage + its reclaimable
    /// `inactive_file` page cache, which the kernel takes back before it
    /// fails an allocation.
    available: Option<u64>,
    /// That level's `memory.pressure` (PSI of the tasks in it), when present.
    pressure: Option<std::path::PathBuf>,
}

fn cgroup_memory() -> Cgroup {
    let Ok(cgroup) = fs::read_to_string("/proc/self/cgroup") else {
        return Cgroup::default();
    };
    let Some(rel) = cgroup.lines().find_map(|l| l.strip_prefix("0::")) else {
        return Cgroup::default();
    };
    cgroup_memory_at(std::path::Path::new("/sys/fs/cgroup"), rel)
}

/// [`cgroup_memory`] for the cgroup `rel` under the hierarchy mounted at
/// `root`.
fn cgroup_memory_at(root: &std::path::Path, rel: &str) -> Cgroup {
    let mut out = Cgroup::default();
    let mut path = root.join(rel.trim_start_matches('/').trim_end_matches('/'));
    loop {
        let num = |file: &str| {
            fs::read_to_string(path.join(file))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
        };
        // `max` means no limit and does not parse: that level is skipped.
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
            out.max = Some(out.max.map_or(max, |m| m.min(max)));
            if out.available.is_none_or(|a| headroom < a) {
                out.available = Some(headroom);
                out.current = Some(current);
                let psi = path.join("memory.pressure");
                out.pressure = psi.is_file().then_some(psi);
            }
        }
        if path == root || !path.pop() {
            break;
        }
    }
    out
}

pub fn host_memory() -> Result<HostMemory> {
    let meminfo = read("/proc/meminfo", "read meminfo")?;
    let cg = cgroup_memory();
    // Stalls of the tasks under the binding limit, when there is one: the
    // host can be idle while this cgroup thrashes against its own limit.
    let psi_path = cg
        .pressure
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("/proc/pressure/memory"));
    let psi = fs::read_to_string(&psi_path).unwrap_or_default();
    let swap_total = required(&meminfo, "SwapTotal")?;
    let raw = LinuxMem {
        mem_free: required(&meminfo, "MemFree")?,
        buffers: required(&meminfo, "Buffers")?,
        cached: required(&meminfo, "Cached")?,
        psi_some_avg10: psi_avg10(&psi, "some"),
        psi_full_avg10: psi_avg10(&psi, "full"),
        psi_from: psi_path.display().to_string(),
        cgroup_max_bytes: cg.max,
        cgroup_current_bytes: cg.current,
        cgroup_available_bytes: cg.available,
    };
    // Inside a memory-limited cgroup the limit is the machine as far as
    // this process is concerned: both totals and availability are the
    // smaller of the host's and the cgroup's.
    let (host_total, host_available) = (
        required(&meminfo, "MemTotal")?,
        required(&meminfo, "MemAvailable")?,
    );
    let total = cg.max.map_or(host_total, |m| host_total.min(m));
    let available = cg
        .available
        .map_or(host_available, |a| host_available.min(a));
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

/// GPUs and their memory, as their drivers report it to an unprivileged
/// user: AMD through amdgpu's sysfs files, NVIDIA through `nvidia-smi`
/// (the NVML library behind it is NVIDIA's; hearth runs the tool rather
/// than linking it). A machine with neither reports none.
pub fn accelerators(query_timeout: std::time::Duration) -> Result<Vec<Accelerator>> {
    let mut out = amd_gpus(std::path::Path::new("/sys/class/drm"))?;
    out.extend(nvidia_gpus(query_timeout)?);
    Ok(out)
}

fn amd_gpus(drm: &std::path::Path) -> Result<Vec<Accelerator>> {
    let entries = match fs::read_dir(drm) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SenseError::Os {
                call: "read_dir /sys/class/drm",
                detail: e.to_string(),
            });
        }
    };
    let mut cards: Vec<std::path::PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        // card0, card1: not their connectors (card0-DP-1).
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix("card"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    cards.sort();
    Ok(cards
        .into_iter()
        .filter_map(|card| {
            let dev = card.join("device");
            let num = |f: &str| {
                fs::read_to_string(dev.join(f))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
            };
            let vram = num("mem_info_vram_total")?;
            let card_name = card.file_name().map(|n| n.to_string_lossy().into_owned());
            let name = fs::read_to_string(dev.join("product_name"))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("AMD GPU {}", card_name.unwrap_or_default()));
            // An APU's VRAM is only the firmware's carve-out; it can use far
            // more system memory through GTT, so it is unified memory.
            let gtt = num("mem_info_gtt_total").filter(|&g| g > vram);
            Some(Accelerator {
                name,
                unified_memory: gtt.is_some(),
                total_bytes: Some(gtt.unwrap_or(vram)),
                recommended_working_set_bytes: None,
                wired_limit_bytes: None,
                used_bytes: num("mem_info_vram_used"),
            })
        })
        .collect())
}

/// NVIDIA GPUs from `nvidia-smi`, which is killed if it has not answered
/// within `timeout`: a wedged driver leaves it stuck in the kernel, and the
/// commands asking must not wait with it. No `nvidia-smi` means no NVIDIA
/// GPU; any other failure (a driver and library mismatch) is an error.
fn nvidia_gpus(timeout: std::time::Duration) -> Result<Vec<Accelerator>> {
    Ok(nvidia_smi(
        &[
            "--query-gpu=name,memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ],
        timeout,
    )?
    .map(|text| parse_nvidia_smi(&text))
    .unwrap_or_default())
}

/// Bytes in a kibibyte, mebibyte and gibibyte: the units drivers report in.
const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

/// Accelerator memory each of `pids` holds: NVIDIA's compute processes, as
/// `nvidia-smi` reports them, plus the device memory other drivers (amdgpu,
/// i915, xe) report in each process's DRM fdinfo. Processes holding none
/// are left out. Each source that could not be read is reported, and the
/// others still count.
///
/// A buffer two processes share is counted in both, so these are upper
/// bounds per process and must not be summed across processes. pids are
/// those of hearth's own pid namespace.
#[must_use]
pub fn accelerator_process_bytes(
    pids: &[i32],
    timeout: std::time::Duration,
) -> (Vec<(i32, u64)>, Vec<String>) {
    let mut errors = Vec::new();
    let nvidia = match nvidia_smi(
        &[
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ],
        timeout,
    ) {
        Ok(text) => text.map(|t| parse_compute_apps(&t)).unwrap_or_default(),
        Err(e) => {
            errors.push(e.to_string());
            Vec::new()
        }
    };
    let mut out = Vec::new();
    for &pid in pids {
        let from_nvidia = nvidia
            .iter()
            .find(|(p, _)| *p == pid)
            .map_or(0, |(_, b)| *b);
        let from_drm = drm_vram(pid).unwrap_or_else(|e| {
            errors.push(e);
            0
        });
        let total = from_nvidia.saturating_add(from_drm);
        if total > 0 {
            out.push((pid, total));
        }
    }
    (out, errors)
}

/// Device memory a process holds through DRM devices, from the fdinfo of
/// its descriptors open on `/dev/dri`: each DRM client once (descriptors
/// can share one). An error when its descriptors cannot be listed.
fn drm_vram(pid: i32) -> std::result::Result<u64, String> {
    let fds = format!("/proc/{pid}/fd");
    let entries = match fs::read_dir(&fds) {
        Ok(e) => e,
        // Gone since it was listed: it holds nothing now.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("{fds}: {e}")),
    };
    let mut clients: Vec<(u64, u64)> = Vec::new();
    for entry in entries.flatten() {
        // A cheap readlink first: most descriptors are sockets and files,
        // whose fdinfo is not read.
        let on_dri =
            fs::read_link(entry.path()).is_ok_and(|target| target.starts_with("/dev/dri/"));
        if !on_dri {
            continue;
        }
        let info = format!("/proc/{pid}/fdinfo/{}", entry.file_name().to_string_lossy());
        let text = match fs::read_to_string(&info) {
            Ok(t) => t,
            // Closed between listing and reading.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{info}: {e}")),
        };
        if let Some((client, bytes)) =
            parse_drm_fdinfo(&text).map_err(|e| format!("{info}: {e}"))?
            && !clients.iter().any(|(c, _)| *c == client)
        {
            clients.push((client, bytes));
        }
    }
    Ok(clients.iter().fold(0, |sum, (_, b)| sum.saturating_add(*b)))
}

/// `(drm-client-id, device memory bytes)` of one DRM fdinfo, if it is one.
/// Resident device memory (`drm-resident-vram0`, `drm-resident-local0`)
/// when the kernel reports it; else the older per-region totals
/// (`drm-memory-vram`), which also count buffers not resident there.
fn parse_drm_fdinfo(text: &str) -> std::result::Result<Option<(u64, u64)>, String> {
    let device = |region: &str| region.starts_with("vram") || region.starts_with("local");
    let mut client = None;
    let (mut resident, mut older) = (None::<u64>, None::<u64>);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key == "drm-client-id" {
            client = value.trim().parse().ok();
            continue;
        }
        let slot = match (
            key.strip_prefix("drm-resident-"),
            key.strip_prefix("drm-memory-"),
        ) {
            (Some(region), _) if device(region) => &mut resident,
            (_, Some(region)) if device(region) => &mut older,
            _ => continue,
        };
        let mut words = value.split_whitespace();
        let Some(n) = words.next().and_then(|w| w.parse::<u64>().ok()) else {
            continue;
        };
        let unit = match words.next() {
            None => 1,
            Some("KiB") => KIB,
            Some("MiB") => MIB,
            Some("GiB") => GIB,
            // A unit this does not know is not guessed at.
            Some(other) => return Err(format!("{key}: unknown unit {other}")),
        };
        *slot = Some(slot.unwrap_or(0).saturating_add(n.saturating_mul(unit)));
    }
    Ok(client.map(|c| (c, resident.or(older).unwrap_or(0))))
}

/// `pid, used_memory` lines in MiB; a process on several GPUs is summed.
fn parse_compute_apps(text: &str) -> Vec<(i32, u64)> {
    let mut out: Vec<(i32, u64)> = Vec::new();
    for line in text.lines() {
        let mut f = line.split(',');
        let (Some(pid), Some(mib)) = (f.next(), f.next()) else {
            continue;
        };
        let (Ok(pid), Ok(mib)) = (pid.trim().parse::<i32>(), mib.trim().parse::<u64>()) else {
            continue;
        };
        let bytes = mib.saturating_mul(MIB);
        match out.iter_mut().find(|(p, _)| *p == pid) {
            Some((_, b)) => *b = b.saturating_add(bytes),
            None => out.push((pid, bytes)),
        }
    }
    out
}

/// Runs `nvidia-smi` with `args`, bounded by `timeout`. `None` when it is
/// not installed (no NVIDIA driver).
fn nvidia_smi(args: &[&str], timeout: std::time::Duration) -> Result<Option<String>> {
    use std::io::Read as _;
    let fail = |detail: String| SenseError::Os {
        call: "nvidia-smi",
        detail,
    };
    let mut child = match std::process::Command::new("nvidia-smi")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(fail(e.to_string())),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut out) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = out.read_to_string(&mut text);
            let _ = tx.send(text);
        });
    }
    let Ok(text) = rx.recv_timeout(timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(fail(format!("did not answer within {timeout:?}; killed")));
    };
    let status = child.wait().map_err(|e| fail(e.to_string()))?;
    if !status.success() {
        let mut err = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        return Err(fail(format!("{status}: {}", err.trim())));
    }
    Ok(Some(text))
}

/// `name, total MiB, used MiB` per line, as nvidia-smi prints them.
fn parse_nvidia_smi(text: &str) -> Vec<Accelerator> {
    let mib = |s: &str| s.trim().parse::<u64>().ok().map(|n| n * 1024 * 1024);
    text.lines()
        .filter_map(|line| {
            let mut f = line.split(',');
            let (name, total, used) = (f.next()?, f.next()?, f.next()?);
            Some(Accelerator {
                name: name.trim().to_string(),
                unified_memory: false,
                total_bytes: Some(mib(total)?),
                recommended_working_set_bytes: None,
                wired_limit_bytes: None,
                used_bytes: mib(used),
            })
        })
        .collect()
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

/// The arguments `pid` was started with, its program first.
pub fn process_args(pid: i32) -> Result<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).map_err(|e| SenseError::Os {
        call: "read /proc/PID/cmdline",
        detail: e.to_string(),
    })?;
    // Each argument ends in a NUL, empty ones included; only the last
    // terminator is not an argument.
    let raw = raw.strip_suffix(&[0]).unwrap_or(&raw);
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    Ok(raw
        .split(|&b| b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_gpu_memory_from_amdgpu_sysfs_and_nvidia_smi() {
        let drm = std::env::temp_dir().join(format!("hearth-drm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&drm);
        let dev = drm.join("card1/device");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(drm.join("card1-DP-1")).unwrap();
        fs::write(dev.join("mem_info_vram_total"), "25753026560\n").unwrap();
        fs::write(dev.join("mem_info_vram_used"), "1073741824\n").unwrap();
        fs::write(dev.join("product_name"), "Radeon RX 7900 XTX\n").unwrap();
        // An APU: a small carve-out, a large GTT.
        let apu = drm.join("card2/device");
        fs::create_dir_all(&apu).unwrap();
        fs::write(apu.join("mem_info_vram_total"), "536870912").unwrap();
        fs::write(apu.join("mem_info_gtt_total"), "68719476736").unwrap();
        let amd = amd_gpus(&drm).unwrap();
        assert_eq!(amd.len(), 2);
        assert_eq!(amd[0].name, "Radeon RX 7900 XTX");
        assert!(!amd[0].unified_memory);
        assert!(amd[1].unified_memory && amd[1].total_bytes == Some(68_719_476_736));
        assert_eq!(amd[1].name, "AMD GPU card2");
        assert_eq!(amd[0].used_bytes, Some(1_073_741_824));
        fs::remove_dir_all(drm).unwrap();
        let nv = parse_nvidia_smi("NVIDIA GeForce RTX 4090, 24564, 1234\nbroken line\n");
        assert_eq!(nv.len(), 1);
        assert_eq!(nv[0].total_bytes, Some(24564 * 1024 * 1024));
        // A process on two GPUs is one entry, summed.
        let apps = parse_compute_apps("4242, 20480\n77, 512\n4242, 1024\nbad\n");
        assert_eq!(apps, [(4242, 21504 * 1024 * 1024), (77, 512 * 1024 * 1024)]);
        let amd = "pos:\t0\nflags:\t02100002\ndrm-driver:\tamdgpu\ndrm-client-id:\t17\ndrm-memory-vram:\t8388608 KiB\ndrm-memory-gtt:\t2048 KiB\n";
        assert_eq!(parse_drm_fdinfo(amd), Ok(Some((17, 8_589_934_592))));
        assert_eq!(parse_drm_fdinfo("pos:\t0\nflags:\t0100002\n"), Ok(None));
        // Newer kernels: resident device memory wins over the totals.
        let xe = "drm-client-id:\t4\ndrm-total-vram0:\t4 GiB\ndrm-resident-vram0:\t3 GiB\ndrm-resident-system:\t1 GiB\ndrm-memory-vram0:\t4 GiB\n";
        assert_eq!(parse_drm_fdinfo(xe), Ok(Some((4, 3 << 30))));
        let odd = "drm-client-id:\t5\ndrm-memory-vram:\t7 TiB\n";
        assert!(parse_drm_fdinfo(odd).is_err());
    }

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
        let cg = cgroup_memory_at(&root, "/slice/app");
        assert_eq!(
            (cg.max, cg.current, cg.available),
            (Some(1000), Some(900), Some(150))
        );
        // A smaller limit with more room still bounds the total.
        w(&leaf, "500", "10", 0);
        let cg = cgroup_memory_at(&root, "/slice/app");
        assert_eq!((cg.max, cg.available), (Some(500), Some(150)));
        assert_eq!(cgroup_memory_at(&root, "/nowhere"), Cgroup::default());
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
