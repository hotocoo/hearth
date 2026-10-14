#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{accelerators, host_memory, list_processes, process_footprint};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{accelerators, host_memory, list_processes, process_footprint};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod unsupported {
    use crate::{Accelerator, HostMemory, ProcessEntry, ProcessFootprint, Result, SenseError};

    pub fn host_memory() -> Result<HostMemory> {
        Err(SenseError::Unsupported("host memory"))
    }
    pub fn process_footprint(_pid: i32) -> Result<ProcessFootprint> {
        Err(SenseError::Unsupported("process footprint"))
    }
    pub fn list_processes() -> Result<Vec<ProcessEntry>> {
        Err(SenseError::Unsupported("process list"))
    }
    pub fn accelerators() -> Result<Vec<Accelerator>> {
        Err(SenseError::Unsupported("accelerators"))
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub use unsupported::{accelerators, host_memory, list_processes, process_footprint};
