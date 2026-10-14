//! Memory pressure levels where the kernel publishes none.
//!
//! macOS declares a level (`kern.memorystatus_vm_pressure_level`) and hearth
//! reports it as is. Linux publishes how much time tasks stalled on memory
//! (PSI) instead; a level is derived from it with the `[pressure]`
//! thresholds, and the derivation says so.

use hearth_core::config::Pressure;
use hearth_sense::{HostMemory, PressureLevel, RawHost};

/// Sets `m.pressure` from PSI when the kernel declared no level.
/// `doctor --explain` describes the derivation.
pub fn fill(m: &mut HostMemory, p: &Pressure) {
    if m.pressure.is_some() {
        return;
    }
    let RawHost::Linux(l) = &m.raw else {
        return;
    };
    let Some(some) = l.psi_some_avg10 else {
        return;
    };
    let full = l.psi_full_avg10.unwrap_or(0.0);
    let level = if full >= p.psi_full_critical {
        PressureLevel::Critical
    } else if some >= p.psi_some_warn {
        PressureLevel::Warn
    } else {
        PressureLevel::Normal
    };
    m.pressure = Some(level);
}

fn config() -> Result<hearth_core::config::Config, String> {
    hearth_core::config::load().map_err(|e| e.to_string())
}

/// `hearth_sense::host_memory`, with the pressure level filled in.
pub fn host_memory() -> Result<HostMemory, String> {
    let p = config()?.pressure;
    let mut m = hearth_sense::host_memory().map_err(|e| e.to_string())?;
    fill(&mut m, &p);
    Ok(m)
}

/// `hearth_sense::box_health`, with the pressure level filled in.
pub fn box_health() -> Result<hearth_sense::BoxHealth, String> {
    let p = config()?.pressure;
    let mut h = hearth_sense::box_health().map_err(|e| e.to_string())?;
    fill(&mut h.memory, &p);
    Ok(h)
}

/// `hearth_sense::accelerators`, with driver queries bounded by
/// `accelerators.query_timeout_ms`.
pub fn accelerators() -> Result<Vec<hearth_sense::Accelerator>, String> {
    let timeout = std::time::Duration::from_millis(config()?.accelerators.query_timeout_ms);
    hearth_sense::accelerators(timeout).map_err(|e| e.to_string())
}

/// `hearth_sense::accelerator_process_bytes`, bounded the same way.
pub fn accelerator_process_bytes(pids: &[i32]) -> Result<Vec<(i32, u64)>, String> {
    let timeout = std::time::Duration::from_millis(config()?.accelerators.query_timeout_ms);
    hearth_sense::accelerator_process_bytes(pids, timeout).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux(some: f64, full: f64) -> HostMemory {
        HostMemory {
            taken_at_ms: 0,
            total_bytes: 1,
            available_bytes: 1,
            swap_total_bytes: 0,
            swap_used_bytes: 0,
            pressure: None,
            raw: RawHost::Linux(hearth_sense::LinuxMem {
                mem_free: 0,
                buffers: 0,
                cached: 0,
                psi_some_avg10: Some(some),
                psi_full_avg10: Some(full),
                psi_from: "/proc/pressure/memory".into(),
                cgroup_max_bytes: None,
                cgroup_current_bytes: None,
                cgroup_available_bytes: None,
            }),
        }
    }

    #[test]
    fn levels_follow_the_configured_thresholds() {
        let p = Pressure {
            psi_some_warn: 10.0,
            psi_full_critical: 20.0,
        };
        for (some, full, want) in [
            (0.5, 0.0, PressureLevel::Normal),
            (12.0, 1.0, PressureLevel::Warn),
            (40.0, 25.0, PressureLevel::Critical),
        ] {
            let mut m = linux(some, full);
            fill(&mut m, &p);
            assert_eq!(m.pressure, Some(want));
        }
        let mut declared = linux(50.0, 50.0);
        declared.pressure = Some(PressureLevel::Normal);
        fill(&mut declared, &p);
        assert_eq!(
            declared.pressure,
            Some(PressureLevel::Normal),
            "a declared level is kept"
        );
    }
}
