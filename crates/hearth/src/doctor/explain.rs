//! `hearth doctor --explain`: where every number in the report comes
//! from, as the counter, sysctl or formula behind it.

use hearth_sense::BoxHealth;
use serde::Serialize;

use crate::units::bytes;

/// One reported number and how it was obtained.
#[derive(Serialize)]
pub(super) struct Derivation {
    pub(super) quantity: &'static str,
    pub(super) value: String,
    pub(super) source: String,
}

/// Where a Linux pressure level comes from: PSI, against the thresholds.
fn linux_pressure_source(l: &hearth_sense::LinuxMem) -> String {
    let pct = |v: Option<f64>| v.map_or("unreadable".into(), |v| format!("{v}%"));
    let (some, full) = (pct(l.psi_some_avg10), pct(l.psi_full_avg10));
    match hearth_core::config::load() {
        Ok(c) => format!(
            "{}: some avg10 {some} (warn at pressure.psi_some_warn = {}), full avg10 {full} (critical at pressure.psi_full_critical = {})",
            l.psi_from, c.pressure.psi_some_warn, c.pressure.psi_full_critical
        ),
        Err(e) => format!(
            "{}: some avg10 {some}, full avg10 {full}; no level, since the configuration does not load: {e}",
            l.psi_from
        ),
    }
}

/// How each memory and limit figure in `h` was obtained on this platform.
pub(super) fn derivations(h: &BoxHealth) -> Vec<Derivation> {
    let d = |quantity, value: String, source: String| Derivation {
        quantity,
        value,
        source,
    };
    let m = &h.memory;
    let macos = matches!(m.raw, hearth_sense::RawHost::Macos(_));
    let (total_from, swap_from) = if macos {
        ("sysctl hw.memsize", "sysctl vm.swapusage")
    } else {
        (
            "/proc/meminfo MemTotal",
            "/proc/meminfo SwapTotal - SwapFree",
        )
    };
    let level = m.pressure.map_or_else(
        || "not published".to_string(),
        |p| format!("{p:?}").to_lowercase(),
    );
    let mut out = vec![d("memory.total", bytes(m.total_bytes), total_from.into())];
    match &m.raw {
        hearth_sense::RawHost::Macos(vm) => {
            let page = |n: u64| bytes(n.saturating_mul(vm.page_size));
            out.push(d(
                "memory.available",
                bytes(m.available_bytes),
                format!(
                    "total - (app + wired + compressed), app = internal - purgeable, the Activity Monitor definition: {} - ({} - {} + {} + {}) from host_statistics64 (page size {})",
                    bytes(m.total_bytes),
                    page(vm.internal),
                    page(vm.purgeable),
                    page(vm.wired),
                    page(vm.compressor_occupied),
                    vm.page_size
                ),
            ));
            out.push(d(
                "memory.pressure",
                level.clone(),
                format!(
                    "sysctl kern.memorystatus_vm_pressure_level = {} (1 normal, 2 warn, 4 critical)",
                    vm.vm_pressure_level.map_or("unreadable".into(), |v| v.to_string())
                ),
            ));
        }
        hearth_sense::RawHost::Linux(l) => {
            out.push(d(
                "memory.available",
                bytes(m.available_bytes),
                match (l.cgroup_available_bytes, l.cgroup_max_bytes) {
                    (Some(room), Some(max)) => format!(
                        "the smaller of /proc/meminfo MemAvailable and the room left under the cgroup limits: {} (limit - usage + reclaimable page cache at the tightest level; smallest limit {})",
                        bytes(room),
                        bytes(max)
                    ),
                    _ => "/proc/meminfo MemAvailable (no cgroup memory limit)".into(),
                },
            ));
            out.push(d(
                "memory.pressure",
                level.clone(),
                linux_pressure_source(l),
            ));
        }
    }
    out.push(d(
        "memory.swap_used",
        bytes(m.swap_used_bytes),
        swap_from.into(),
    ));
    let l = &h.limits;
    let shown = |v: Option<u64>| v.map_or("unknown".into(), |n| n.to_string());
    for (field, value) in [
        ("files_open", l.files_open),
        ("threads_total", l.threads_total),
        ("files_max", l.files_max),
        ("files_per_process_max", l.files_per_process_max),
        ("threads_max", l.threads_max),
        ("threads_per_process_max", l.threads_per_process_max),
        ("processes_max", l.processes_max),
    ] {
        let source = hearth_sense::LIMIT_SOURCES
            .iter()
            .find(|(f, _)| *f == field)
            .map_or("not measured on this platform", |(_, s)| s);
        out.push(d(
            hearth_sense::LIMIT_SOURCES
                .iter()
                .find(|(f, _)| *f == field)
                .map_or(field, |(f, _)| f),
            shown(value),
            source.to_string(),
        ));
    }
    out
}
