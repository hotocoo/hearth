#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::effective_uid;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{
    LIMIT_SOURCES, accelerator_process_bytes, accelerators, host_memory, list_processes,
    process_args, process_counts, process_footprint, process_group, system_limits, tcp_sockets,
};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{
    LIMIT_SOURCES, accelerator_process_bytes, accelerators, host_memory, list_processes,
    process_args, process_counts, process_footprint, process_group, system_limits, tcp_sockets,
};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod unsupported {
    use crate::{
        Accelerator, HostMemory, ProcessCounts, ProcessEntry, ProcessFootprint, Result, SenseError,
        SystemLimits, TcpSocket,
    };

    pub fn host_memory() -> Result<HostMemory> {
        Err(SenseError::Unsupported("host memory"))
    }
    pub fn process_footprint(_pid: i32) -> Result<ProcessFootprint> {
        Err(SenseError::Unsupported("process footprint"))
    }
    pub fn list_processes() -> Result<Vec<ProcessEntry>> {
        Err(SenseError::Unsupported("process list"))
    }
    pub fn accelerators(_query_timeout: std::time::Duration) -> Result<Vec<Accelerator>> {
        Err(SenseError::Unsupported("accelerators"))
    }
    pub fn accelerator_process_bytes(
        _pids: &[i32],
        _query_timeout: std::time::Duration,
    ) -> (Vec<(i32, u64)>, Vec<String>) {
        (
            Vec::new(),
            vec!["accelerator memory per process is not read on this platform".into()],
        )
    }
    pub fn system_limits() -> Result<SystemLimits> {
        Err(SenseError::Unsupported("system limits"))
    }
    pub fn process_counts(_pid: i32) -> Result<ProcessCounts> {
        Err(SenseError::Unsupported("process counts"))
    }
    pub fn tcp_sockets(_pid: i32) -> Result<Vec<TcpSocket>> {
        Err(SenseError::Unsupported("tcp sockets"))
    }
    pub const LIMIT_SOURCES: &[(&str, &str)] = &[];
    pub fn process_args(_pid: i32) -> Result<Vec<String>> {
        Err(SenseError::Unsupported("process arguments"))
    }
    pub fn process_group(_pid: i32) -> Result<i32> {
        Err(SenseError::Unsupported("process groups"))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use unsupported::{
    LIMIT_SOURCES, accelerator_process_bytes, accelerators, host_memory, list_processes,
    process_args, process_counts, process_footprint, process_group, system_limits, tcp_sockets,
};
