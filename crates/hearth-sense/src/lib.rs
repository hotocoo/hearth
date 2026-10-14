//! Measured memory sensing for hearth.
//!
//! Every value here is read from the operating system at call time. Nothing is
//! estimated or defaulted: estimation and policy belong to the admission layer,
//! which needs to know the difference between what was measured and what was
//! guessed.

mod sys;

use serde::Serialize;

pub use sys::{accelerators, host_memory, list_processes, process_footprint};

#[derive(Debug, thiserror::Error)]
pub enum SenseError {
    #[error("{call} failed: {detail}")]
    Os { call: &'static str, detail: String },
    #[error("process {0} not found")]
    NoProcess(i32),
    #[error("not permitted to inspect process {0}")]
    NotPermitted(i32),
    #[error("parse {what}: {detail}")]
    Parse { what: &'static str, detail: String },
    #[error("not supported on this platform: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, SenseError>;

/// One reading of host memory.
#[derive(Debug, Clone, Serialize)]
pub struct HostMemory {
    pub taken_at_ms: u64,
    pub total_bytes: u64,
    /// Memory the OS reports as obtainable without swapping. On Linux this is
    /// `MemAvailable`. On macOS it is `total - (app + wired + compressed)`, the
    /// "Memory Used" definition Activity Monitor uses; see `raw` for inputs.
    pub available_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
    /// Level the kernel itself publishes, when it publishes one.
    pub pressure: Option<PressureLevel>,
    pub raw: RawHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressureLevel {
    Normal,
    Warn,
    Critical,
}

/// Unprocessed kernel counters, kept so alternative formulas can be evaluated
/// against recordings without re-running an experiment.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "os", rename_all = "snake_case")]
pub enum RawHost {
    Macos(MacVm),
    Linux(LinuxMem),
}

/// `host_statistics64(HOST_VM_INFO64)` plus memorystatus sysctls. Page counts
/// are in units of `page_size`.
#[derive(Debug, Clone, Serialize)]
pub struct MacVm {
    pub page_size: u64,
    pub free: u64,
    pub active: u64,
    pub inactive: u64,
    pub speculative: u64,
    pub throttled: u64,
    pub wired: u64,
    pub purgeable: u64,
    pub external: u64,
    pub internal: u64,
    pub compressor_occupied: u64,
    pub compressor_uncompressed: u64,
    pub pageouts: u64,
    pub swapins: u64,
    pub swapouts: u64,
    /// `kern.memorystatus_level`: percent of memory the kernel considers free.
    pub memorystatus_level: Option<u64>,
    /// `kern.memorystatus_vm_pressure_level`: 1 normal, 2 warn, 4 critical.
    pub vm_pressure_level: Option<u64>,
}

/// `/proc/meminfo` and `/proc/pressure/memory`, in bytes and PSI percentages.
#[derive(Debug, Clone, Serialize)]
pub struct LinuxMem {
    pub mem_free: u64,
    pub buffers: u64,
    pub cached: u64,
    pub psi_some_avg10: Option<f64>,
    pub psi_full_avg10: Option<f64>,
    pub cgroup_max_bytes: Option<u64>,
    pub cgroup_current_bytes: Option<u64>,
}

/// Memory charged to one process.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessFootprint {
    pub pid: i32,
    /// macOS `ri_phys_footprint` (what jetsam charges); Linux `Pss`.
    pub footprint_bytes: u64,
    pub resident_bytes: u64,
    /// macOS `ri_wired_size`; not exposed per process on Linux.
    pub wired_bytes: Option<u64>,
    /// macOS `ri_lifetime_max_phys_footprint`; Linux `VmHWM`.
    pub peak_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessEntry {
    pub pid: i32,
    /// Kernel process name (macOS `proc_name`, Linux `comm`).
    pub name: String,
    /// Executable path, when the OS can still resolve it.
    pub path: Option<String>,
}

impl ProcessEntry {
    /// True when `pattern` occurs in the name or the path.
    #[must_use]
    pub fn matches(&self, pattern: &str) -> bool {
        self.name.contains(pattern) || self.path.as_deref().is_some_and(|p| p.contains(pattern))
    }
}

/// An accelerator and the limits its driver advertises.
#[derive(Debug, Clone, Serialize)]
pub struct Accelerator {
    pub name: String,
    /// True when the accelerator draws from host RAM (Apple Silicon).
    pub unified_memory: bool,
    pub total_bytes: Option<u64>,
    /// Metal `recommendedMaxWorkingSetSize`.
    pub recommended_working_set_bytes: Option<u64>,
    /// `iogpu.wired_limit_mb` when an override is set; `None` means the kernel
    /// default applies.
    pub wired_limit_bytes: Option<u64>,
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
