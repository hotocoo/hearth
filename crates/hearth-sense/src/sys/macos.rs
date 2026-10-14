//! macOS sensing through `host_statistics64`, `sysctlbyname`,
//! `proc_pid_rusage` and Metal. Needs no elevated privileges.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::mem::MaybeUninit;
use std::sync::OnceLock;

use crate::{
    Accelerator, HostMemory, MacVm, PressureLevel, ProcessCounts, ProcessEntry, ProcessFootprint,
    RawHost, Result, SenseError, SystemLimits, TcpSocket, TcpState, now_ms,
};

fn os_error(call: &'static str) -> SenseError {
    SenseError::Os {
        call,
        detail: std::io::Error::last_os_error().to_string(),
    }
}

/// Reads an integer sysctl. The kernel publishes some as 32-bit and some as
/// 64-bit values, so the width is taken from the size it reports.
fn sysctl_uint(name: &str) -> Result<u64> {
    let cname = CString::new(name).map_err(|e| SenseError::Parse {
        what: "sysctl name",
        detail: e.to_string(),
    })?;
    let mut buf = [0u8; 8];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `cname` is a valid NUL-terminated string, `buf` is writable for
    // `len` bytes and `len` holds its capacity; no new value is written.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(os_error("sysctlbyname"));
    }
    match len {
        4 => Ok(u64::from(u32::from_ne_bytes([
            buf[0], buf[1], buf[2], buf[3],
        ]))),
        8 => Ok(u64::from_ne_bytes(buf)),
        other => Err(SenseError::Parse {
            what: "sysctl width",
            detail: format!("{name} returned {other} bytes"),
        }),
    }
}

fn swap_usage() -> Result<libc::xsw_usage> {
    let mut usage = MaybeUninit::<libc::xsw_usage>::zeroed();
    let mut len: libc::size_t = size_of::<libc::xsw_usage>();
    // SAFETY: the name literal is NUL-terminated and `usage` is a writable
    // buffer of exactly `len` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            c"vm.swapusage".as_ptr(),
            usage.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(os_error("sysctlbyname(vm.swapusage)"));
    }
    // SAFETY: the kernel filled the struct; it is plain integers and was zeroed.
    Ok(unsafe { usage.assume_init() })
}

fn host_port() -> libc::mach_port_t {
    // `mach_host_self` hands out a send right each call; take it once.
    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
    *PORT.get_or_init(|| {
        // SAFETY: no preconditions; returns the host name port for this task.
        #[allow(deprecated)]
        unsafe {
            libc::mach_host_self()
        }
    })
}

fn page_size() -> Result<u64> {
    // SAFETY: sysconf has no preconditions.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).map_err(|_| os_error("sysconf(_SC_PAGESIZE)"))
}

fn vm_statistics() -> Result<libc::vm_statistics64> {
    let mut stats = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `stats` is writable for HOST_VM_INFO64_COUNT integers, which is
    // what `count` declares; the kernel writes at most that many.
    let kr = unsafe {
        libc::host_statistics64(
            host_port(),
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr().cast(),
            &raw mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return Err(SenseError::Os {
            call: "host_statistics64",
            detail: format!("kern_return_t {kr}"),
        });
    }
    // SAFETY: zero-initialised plain-integer struct, filled by the kernel.
    Ok(unsafe { stats.assume_init() })
}

pub fn host_memory() -> Result<HostMemory> {
    let total = sysctl_uint("hw.memsize")?;
    let page = page_size()?;
    let vm = vm_statistics()?;
    let swap = swap_usage()?;
    let raw = MacVm {
        page_size: page,
        free: u64::from(vm.free_count),
        active: u64::from(vm.active_count),
        inactive: u64::from(vm.inactive_count),
        speculative: u64::from(vm.speculative_count),
        throttled: u64::from(vm.throttled_count),
        wired: u64::from(vm.wire_count),
        purgeable: u64::from(vm.purgeable_count),
        external: u64::from(vm.external_page_count),
        internal: u64::from(vm.internal_page_count),
        compressor_occupied: u64::from(vm.compressor_page_count),
        compressor_uncompressed: vm.total_uncompressed_pages_in_compressor,
        pageouts: vm.pageouts,
        swapins: vm.swapins,
        swapouts: vm.swapouts,
        memorystatus_level: sysctl_uint("kern.memorystatus_level").ok(),
        vm_pressure_level: sysctl_uint("kern.memorystatus_vm_pressure_level").ok(),
    };
    let used = mac_used_bytes(&raw);
    Ok(HostMemory {
        taken_at_ms: now_ms(),
        total_bytes: total,
        available_bytes: total.saturating_sub(used),
        swap_total_bytes: swap.xsu_total,
        swap_used_bytes: swap.xsu_used,
        pressure: raw.vm_pressure_level.and_then(pressure_from_level),
        raw: RawHost::Macos(raw),
    })
}

/// Activity Monitor's "Memory Used": app memory (anonymous pages that are not
/// purgeable) plus wired plus pages occupied by the compressor.
pub(crate) fn mac_used_bytes(vm: &MacVm) -> u64 {
    let app = vm.internal.saturating_sub(vm.purgeable);
    (app + vm.wired + vm.compressor_occupied) * vm.page_size
}

fn pressure_from_level(level: u64) -> Option<PressureLevel> {
    // Values from <sys/kern_memorystatus.h> kVMPressure*.
    match level {
        1 => Some(PressureLevel::Normal),
        2 => Some(PressureLevel::Warn),
        4 => Some(PressureLevel::Critical),
        _ => None,
    }
}

pub fn process_footprint(pid: i32) -> Result<ProcessFootprint> {
    let mut info = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `info` is a writable rusage_info_v4, matching RUSAGE_INFO_V4.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    if rc != 0 {
        return Err(match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => SenseError::NoProcess(pid),
            Some(libc::EPERM) => SenseError::NotPermitted(pid),
            _ => os_error("proc_pid_rusage"),
        });
    }
    // SAFETY: zero-initialised plain-integer struct, filled by the kernel.
    let info = unsafe { info.assume_init() };
    Ok(ProcessFootprint {
        pid,
        footprint_bytes: info.ri_phys_footprint,
        resident_bytes: info.ri_resident_size,
        wired_bytes: Some(info.ri_wired_size),
        peak_bytes: Some(info.ri_lifetime_max_phys_footprint),
    })
}

pub fn list_processes() -> Result<Vec<ProcessEntry>> {
    // SAFETY: a null buffer asks only for the current count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut capacity = usize::try_from(count).map_err(|_| os_error("proc_listallpids"))?;
    // Processes can start between calls. A completely filled buffer may have
    // been truncated, so grow and retry until the list fits with room left.
    let pids = loop {
        let mut pids = vec![0i32; capacity.max(1) * 2];
        let bytes = i32::try_from(pids.len() * size_of::<i32>())
            .map_err(|_| os_error("proc_listallpids"))?;
        // SAFETY: `pids` is writable for `bytes` bytes.
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let filled = usize::try_from(filled).map_err(|_| os_error("proc_listallpids"))?;
        if filled < pids.len() {
            pids.truncate(filled);
            break pids;
        }
        capacity = pids.len();
    };
    Ok(pids
        .into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(|pid| {
            let name = pid_name(pid);
            let path = pid_path(pid);
            (name.is_some() || path.is_some()).then(|| ProcessEntry {
                pid,
                name: name.unwrap_or_default(),
                path,
            })
        })
        .collect())
}

/// The kernel's process name. Unlike the path, it survives the executable
/// being replaced on disk (for example a package upgrade under a running
/// server), which makes `proc_pidpath` fail.
fn pid_name(pid: i32) -> Option<String> {
    // MAXCOMLEN * 2, the size of `pbi_name`.
    let mut buf = vec![0u8; libc::MAXCOMLEN * 2 + 1];
    let cap = u32::try_from(buf.len()).ok()?;
    // SAFETY: `buf` is writable for `cap` bytes.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), cap) };
    let len = usize::try_from(len).ok().filter(|&l| l > 0)?;
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

fn pid_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let cap = u32::try_from(buf.len()).ok()?;
    // SAFETY: `buf` is writable for `cap` bytes.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), cap) };
    let len = usize::try_from(len).ok().filter(|&l| l > 0)?;
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

pub fn accelerators() -> Result<Vec<Accelerator>> {
    // Creating a Metal device allocates driver state; the limits never change
    // while the machine is up, so read them once.
    static METAL: OnceLock<Option<(String, bool, u64)>> = OnceLock::new();
    let Some((name, unified, recommended)) = METAL.get_or_init(metal_limits).clone() else {
        return Ok(Vec::new());
    };
    let wired_limit_mb = sysctl_uint("iogpu.wired_limit_mb").ok();
    Ok(vec![Accelerator {
        name,
        unified_memory: unified,
        total_bytes: if unified {
            sysctl_uint("hw.memsize").ok()
        } else {
            None
        },
        recommended_working_set_bytes: Some(recommended),
        wired_limit_bytes: wired_limit_mb
            .filter(|&mb| mb > 0)
            .map(|mb| mb * 1024 * 1024),
        used_bytes: None,
    }])
}

unsafe extern "C" {
    // <mach/mach_host.h> and <mach/processor_set.h>; not in the libc crate.
    fn processor_set_default(
        host: libc::mach_port_t,
        default_set: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
    fn processor_set_statistics(
        pset: libc::mach_port_t,
        flavor: libc::c_int,
        info_out: *mut libc::integer_t,
        info_out_count: *mut libc::mach_msg_type_number_t,
    ) -> libc::kern_return_t;
}

/// Thread count of the default processor set, which is every thread on the
/// machine: the figure `top` shows, readable without privileges.
fn system_threads() -> Option<u64> {
    static PSET: OnceLock<Option<libc::mach_port_t>> = OnceLock::new();
    let pset = (*PSET.get_or_init(|| {
        let mut pset: libc::mach_port_t = 0;
        // SAFETY: `pset` is a valid out-pointer for one port name.
        let kr = unsafe { processor_set_default(host_port(), &raw mut pset) };
        (kr == libc::KERN_SUCCESS).then_some(pset)
    }))?;
    let mut info = MaybeUninit::<libc::processor_set_load_info>::zeroed();
    let mut count = libc::mach_msg_type_number_t::try_from(
        size_of::<libc::processor_set_load_info>() / size_of::<libc::integer_t>(),
    )
    .ok()?;
    // SAFETY: `info` is writable for `count` integers, as `count` declares.
    let kr = unsafe {
        processor_set_statistics(
            pset,
            libc::PROCESSOR_SET_LOAD_INFO,
            info.as_mut_ptr().cast(),
            &raw mut count,
        )
    };
    if kr != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: zero-initialised plain-integer struct, filled by the kernel.
    let info = unsafe { info.assume_init() };
    u64::try_from(info.thread_count).ok()
}

/// Where each [`SystemLimits`] field comes from on this platform.
pub const LIMIT_SOURCES: &[(&str, &str)] = &[
    ("files_open", "sysctl kern.num_files"),
    (
        "threads_total",
        "processor_set_statistics: threads of the default set (what top shows)",
    ),
    ("files_max", "sysctl kern.maxfiles"),
    ("files_per_process_max", "sysctl kern.maxfilesperproc"),
    (
        "threads_max",
        "sysctl kern.num_threads (the kernel's thread limit)",
    ),
    ("threads_per_process_max", "sysctl kern.num_taskthreads"),
    ("processes_max", "sysctl kern.maxproc"),
];

pub fn system_limits() -> Result<SystemLimits> {
    Ok(SystemLimits {
        files_open: sysctl_uint("kern.num_files").ok(),
        threads_total: system_threads(),
        files_max: sysctl_uint("kern.maxfiles").ok(),
        files_per_process_max: sysctl_uint("kern.maxfilesperproc").ok(),
        threads_max: sysctl_uint("kern.num_threads").ok(),
        threads_per_process_max: sysctl_uint("kern.num_taskthreads").ok(),
        processes_max: sysctl_uint("kern.maxproc").ok(),
    })
}

pub fn process_counts(pid: i32) -> Result<ProcessCounts> {
    Ok(ProcessCounts {
        pid,
        open_files: open_files(pid),
        threads: threads(pid),
    })
}

/// The descriptor table of a process.
fn fd_list(pid: i32) -> Option<Vec<libc::proc_fdinfo>> {
    // SAFETY: a null buffer asks only for the size of the descriptor table.
    let size =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let size = usize::try_from(size).ok().filter(|&s| s > 0)?;
    let entry = size_of::<libc::proc_fdinfo>();
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        size.div_ceil(entry)
    ];
    let cap = i32::try_from(fds.len() * entry).ok()?;
    // SAFETY: `fds` is initialised and writable for `cap` bytes.
    let filled =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), cap) };
    fds.truncate(usize::try_from(filled).ok()? / entry);
    Some(fds)
}

fn open_files(pid: i32) -> Option<u64> {
    u64::try_from(fd_list(pid)?.len()).ok()
}

/// `insi_vflag` bits from `<sys/proc_info.h>`.
const INI_IPV4: u8 = 0x1;
const INI_IPV6: u8 = 0x2;

pub fn tcp_sockets(pid: i32) -> Result<Vec<TcpSocket>> {
    use libproc::libproc::file_info::pidfdinfo;
    use libproc::libproc::net_info::{SocketFDInfo, SocketInfoKind};

    let fds = fd_list(pid).ok_or(SenseError::NotPermitted(pid))?;
    let socket_type = u32::try_from(libc::PROX_FDTYPE_SOCKET).unwrap_or(u32::MAX);
    Ok(fds
        .iter()
        .filter(|fd| fd.proc_fdtype == socket_type)
        .filter_map(|fd| pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd).ok())
        .filter(|info| matches!(SocketInfoKind::from(info.psi.soi_kind), SocketInfoKind::Tcp))
        .filter_map(|info| {
            // SAFETY: `soi_kind` is Tcp, so the kernel filled `pri_tcp`.
            let tcp = unsafe { info.psi.soi_proto.pri_tcp };
            let ini = tcp.tcpsi_ini;
            let addr = |a: libproc::libproc::net_info::InSIAddr| -> Option<std::net::IpAddr> {
                if ini.insi_vflag & INI_IPV4 != 0 {
                    // SAFETY: the IPv4 flag means the v4 member was written.
                    let raw = unsafe { a.ina_46.i46a_addr4.s_addr };
                    Some(std::net::Ipv4Addr::from(u32::from_be(raw)).into())
                } else if ini.insi_vflag & INI_IPV6 != 0 {
                    // SAFETY: the IPv6 flag means the v6 member was written.
                    let raw = unsafe { a.ina_6.s6_addr };
                    Some(std::net::Ipv6Addr::from(raw).into())
                } else {
                    None
                }
            };
            let port = |p: libc::c_int| u16::from_be(u16::try_from(p & 0xffff).unwrap_or(0));
            let state = mac_tcp_state(tcp.tcpsi_state);
            let local = std::net::SocketAddr::new(addr(ini.insi_laddr)?, port(ini.insi_lport));
            let remote = (state != TcpState::Listen)
                .then(|| addr(ini.insi_faddr))
                .flatten()
                .map(|ip| std::net::SocketAddr::new(ip, port(ini.insi_fport)));
            Some(TcpSocket {
                local,
                remote,
                state,
            })
        })
        .collect())
}

/// `TCPS_*` values from `<netinet/tcp_fsm.h>`.
fn mac_tcp_state(state: libc::c_int) -> TcpState {
    match state {
        0 => TcpState::Closed,
        1 => TcpState::Listen,
        2 => TcpState::SynSent,
        3 => TcpState::SynReceived,
        4 => TcpState::Established,
        5 => TcpState::CloseWait,
        6 => TcpState::FinWait1,
        7 => TcpState::Closing,
        8 => TcpState::LastAck,
        9 => TcpState::FinWait2,
        10 => TcpState::TimeWait,
        _ => TcpState::Unknown,
    }
}

fn threads(pid: i32) -> Option<u64> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_taskinfo>()).ok()?;
    // SAFETY: `info` is writable for exactly `size` bytes.
    let filled = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if filled != size {
        return None;
    }
    // SAFETY: the kernel filled the whole zero-initialised struct.
    let info = unsafe { info.assume_init() };
    u64::try_from(info.pti_threadnum).ok()
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {}

fn metal_limits() -> Option<(String, bool, u64)> {
    use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
    let device = MTLCreateSystemDefaultDevice()?;
    Some((
        device.name().to_string(),
        device.hasUnifiedMemory(),
        device.recommendedMaxWorkingSetSize(),
    ))
}

/// The process group `pid` belongs to.
pub fn process_group(pid: i32) -> Result<i32> {
    use libproc::libproc::bsd_info::BSDInfo;
    use libproc::libproc::proc_pid::pidinfo;
    let info = pidinfo::<BSDInfo>(pid, 0).map_err(|detail| SenseError::Os {
        call: "proc_pidinfo PROC_PIDTBSDINFO",
        detail,
    })?;
    i32::try_from(info.pbi_pgid).map_err(|e| SenseError::Os {
        call: "proc_pidinfo PROC_PIDTBSDINFO",
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_bytes_follows_activity_monitor_definition() {
        let vm = MacVm {
            page_size: 16384,
            free: 0,
            active: 0,
            inactive: 0,
            speculative: 0,
            throttled: 0,
            wired: 10,
            purgeable: 2,
            external: 0,
            internal: 7,
            compressor_occupied: 3,
            compressor_uncompressed: 0,
            pageouts: 0,
            swapins: 0,
            swapouts: 0,
            memorystatus_level: None,
            vm_pressure_level: None,
        };
        assert_eq!(mac_used_bytes(&vm), (5 + 10 + 3) * 16384);
    }

    #[test]
    fn pressure_levels_map_kernel_constants() {
        assert_eq!(pressure_from_level(1), Some(PressureLevel::Normal));
        assert_eq!(pressure_from_level(2), Some(PressureLevel::Warn));
        assert_eq!(pressure_from_level(4), Some(PressureLevel::Critical));
        assert_eq!(pressure_from_level(3), None);
    }

    #[test]
    fn live_host_reading_is_plausible() {
        let host = host_memory().unwrap();
        assert!(host.total_bytes > 0);
        assert!(host.available_bytes <= host.total_bytes);
    }

    #[test]
    fn live_own_footprint_is_readable() {
        let fp = process_footprint(std::process::id().cast_signed()).unwrap();
        assert!(fp.footprint_bytes > 0);
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
    fn missing_process_is_typed() {
        // pid_max on macOS is 99998; this pid cannot exist.
        assert!(matches!(
            process_footprint(i32::MAX),
            Err(SenseError::NoProcess(_))
        ));
    }
}
