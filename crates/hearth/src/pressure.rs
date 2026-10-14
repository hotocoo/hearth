//! Memory pressure levels where the kernel publishes none.
//!
//! macOS declares a level (`kern.memorystatus_vm_pressure_level`) and hearth
//! reports it as is. Linux publishes how much time tasks stalled on memory
//! (PSI) instead; a level is derived from it with the `[pressure]`
//! thresholds, and the derivation says so.

use hearth_core::config::Pressure;
use hearth_sense::{HostMemory, PressureLevel, RawHost};

/// Sets `m.pressure` from PSI when the kernel declared no level. Returns
/// how the level was derived, for `doctor --explain`, when it was.
pub fn fill(m: &mut HostMemory, p: &Pressure) -> Option<String> {
    if m.pressure.is_some() {
        return None;
    }
    let RawHost::Linux(l) = &m.raw else {
        return None;
    };
    let (some, full) = (l.psi_some_avg10?, l.psi_full_avg10.unwrap_or(0.0));
    let level = if full >= p.psi_full_critical {
        PressureLevel::Critical
    } else if some >= p.psi_some_warn {
        PressureLevel::Warn
    } else {
        PressureLevel::Normal
    };
    m.pressure = Some(level);
    Some(format!(
        "derived from PSI: some avg10 {some} (warn at {}), full avg10 {full} (critical at {}), pressure.psi_* in the config",
        p.psi_some_warn, p.psi_full_critical
    ))
}

fn thresholds() -> Option<Pressure> {
    hearth_core::config::load().ok().map(|c| c.pressure)
}

/// `hearth_sense::host_memory`, with the pressure level filled in.
pub fn host_memory() -> Result<HostMemory, String> {
    let mut m = hearth_sense::host_memory().map_err(|e| e.to_string())?;
    if let Some(p) = thresholds() {
        fill(&mut m, &p);
    }
    Ok(m)
}

/// `hearth_sense::box_health`, with the pressure level filled in.
pub fn box_health() -> Result<hearth_sense::BoxHealth, String> {
    let mut h = hearth_sense::box_health().map_err(|e| e.to_string())?;
    if let Some(p) = thresholds() {
        fill(&mut h.memory, &p);
    }
    Ok(h)
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
                cgroup_max_bytes: None,
                cgroup_current_bytes: None,
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
            assert!(fill(&mut m, &p).is_some());
            assert_eq!(m.pressure, Some(want));
        }
        let mut declared = linux(50.0, 50.0);
        declared.pressure = Some(PressureLevel::Normal);
        assert!(
            fill(&mut declared, &p).is_none(),
            "a declared level is kept"
        );
    }
}
