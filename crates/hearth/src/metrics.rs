//! Prometheus metrics from hearthd, for Grafana or any scraper: the state
//! of each server hearthd keeps, and the machine's memory and kernel-wide
//! counts, measured at each scrape.
//!
//! Off unless `metrics.listen` is set. The endpoint is read-only and says
//! nothing that is not already in `hearth status` and `hearth doctor`.
//! Labels are bounded: server names from the configuration, and states from
//! a fixed set.

use std::fmt::Write as _;
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Every state a server can be in, so each gets a 0 or 1 series.
const STATES: &[&str] = &[
    "starting",
    "ready",
    "waiting",
    "stopped",
    "gave_up",
    "unsupervised",
];

/// One label value, escaped as the text format requires.
fn label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Declares one metric family.
fn family(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// The exposition for `servers` (as `hearth status` reports them) and the
/// machine as measured now.
pub fn render(servers: &[Value]) -> String {
    let mut out = String::new();
    let name_of = |s: &Value| label(s["name"].as_str().unwrap_or("?"));
    family(
        &mut out,
        "hearth_server_state",
        "gauge",
        "1 for the state each server hearthd keeps is in.",
    );
    for s in servers {
        let state = s["state"].as_str().unwrap_or("?");
        for candidate in STATES {
            let _ = writeln!(
                out,
                "hearth_server_state{{server=\"{}\",state=\"{candidate}\"}} {}",
                name_of(s),
                u8::from(*candidate == state)
            );
        }
    }
    family(
        &mut out,
        "hearth_server_restarts_total",
        "counter",
        "Restarts hearthd decided for each server.",
    );
    for s in servers {
        let _ = writeln!(
            out,
            "hearth_server_restarts_total{{server=\"{}\"}} {}",
            name_of(s),
            s["restarts"].as_u64().unwrap_or_default()
        );
    }
    family(
        &mut out,
        "hearth_server_exit_cause",
        "gauge",
        "1 for why each server last exited, when its log said: model_missing, load_failed, out_of_memory, port_in_use or bad_argument. All 0 while its last event is not such an exit.",
    );
    for s in servers {
        let last = &s["last_event"];
        let cause = (last["event"] == "exited")
            .then(|| last["cause"].as_str())
            .flatten();
        for candidate in hearth_runtime::signals::Cause::ALL.iter().map(|c| c.name()) {
            let _ = writeln!(
                out,
                "hearth_server_exit_cause{{server=\"{}\",cause=\"{candidate}\"}} {}",
                name_of(s),
                u8::from(cause == Some(candidate))
            );
        }
    }
    family(
        &mut out,
        "hearth_server_footprint_bytes",
        "gauge",
        "Memory footprint of each server's process, as the kernel charges it.",
    );
    for s in servers {
        let footprint = s["server_pid"]
            .as_i64()
            .and_then(|p| i32::try_from(p).ok())
            .and_then(|p| hearth_sense::process_footprint(p).ok());
        if let Some(f) = footprint {
            let _ = writeln!(
                out,
                "hearth_server_footprint_bytes{{server=\"{}\"}} {}",
                name_of(s),
                f.footprint_bytes
            );
        }
    }
    machine(&mut out);
    out
}

fn machine(out: &mut String) {
    if let Ok(m) = crate::pressure::host_memory() {
        for (name, help, value) in [
            (
                "hearth_memory_total_bytes",
                "Physical memory.",
                m.total_bytes,
            ),
            (
                "hearth_memory_available_bytes",
                "Memory obtainable without swapping.",
                m.available_bytes,
            ),
            ("hearth_swap_used_bytes", "Swap in use.", m.swap_used_bytes),
        ] {
            family(out, name, "gauge", help);
            let _ = writeln!(out, "{name} {value}");
        }
        if let Some(level) = m.pressure {
            family(
                out,
                "hearth_memory_pressure",
                "gauge",
                "Kernel memory pressure: 0 normal, 1 warn, 2 critical.",
            );
            let level = match level {
                hearth_sense::PressureLevel::Normal => 0,
                hearth_sense::PressureLevel::Warn => 1,
                hearth_sense::PressureLevel::Critical => 2,
            };
            let _ = writeln!(out, "hearth_memory_pressure {level}");
        }
    }
    let gpus: Vec<hearth_sense::Accelerator> = match crate::pressure::accelerators() {
        Ok(all) => all.into_iter().filter(|a| !a.unified_memory).collect(),
        Err(e) => {
            // A comment line: scrapers ignore it, a person reading sees why.
            let _ = writeln!(out, "# gpu memory not read: {}", e.replace('\n', " "));
            Vec::new()
        }
    };
    for (name, help, pick) in [
        (
            "hearth_gpu_memory_total_bytes",
            "Memory of each discrete GPU.",
            (|a: &hearth_sense::Accelerator| a.total_bytes)
                as fn(&hearth_sense::Accelerator) -> Option<u64>,
        ),
        (
            "hearth_gpu_memory_used_bytes",
            "Memory in use on each discrete GPU, as its driver reports it.",
            |a: &hearth_sense::Accelerator| a.used_bytes,
        ),
    ] {
        if gpus.iter().any(|a| pick(a).is_some()) {
            family(out, name, "gauge", help);
            // Indexed: two identical cards would otherwise be two series
            // with one label set, and the scraper would reject the page.
            for (index, a) in gpus.iter().enumerate() {
                if let Some(v) = pick(a) {
                    let _ = writeln!(
                        out,
                        "{name}{{gpu=\"{}\",index=\"{index}\"}} {v}",
                        label(&a.name)
                    );
                }
            }
        }
    }
    if let Ok(l) = hearth_sense::system_limits() {
        for (name, help, value) in [
            (
                "hearth_open_files",
                "Open files on this machine.",
                l.files_open,
            ),
            (
                "hearth_open_files_max",
                "The kernel's open-file limit.",
                l.files_max,
            ),
            (
                "hearth_threads",
                "Threads on this machine.",
                l.threads_total,
            ),
            (
                "hearth_threads_max",
                "The kernel's thread limit.",
                l.threads_max,
            ),
        ] {
            if let Some(v) = value {
                family(out, name, "gauge", help);
                let _ = writeln!(out, "{name} {v}");
            }
        }
    }
}

/// Answers one scrape. Anything but `GET /metrics` is a 404.
fn answer(
    stream: TcpStream,
    config: &hearth_core::config::Control,
    servers: &dyn Fn() -> Vec<Value>,
) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
    let head = crate::listen::read_head(&stream, config.max_message_bytes, deadline)?;
    let (status, body) = if head.starts_with("GET /metrics ") {
        ("200 OK", render(&servers()))
    } else {
        ("404 Not Found", "only GET /metrics is served\n".to_string())
    };
    stream.set_write_timeout(Some(
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .unwrap_or(Duration::from_millis(config.timeout_ms)),
    ))?;
    crate::listen::respond(stream, status, "text/plain; version=0.0.4", "", &body)
}

/// Serves metrics on `listener` until hearthd ends. Each scrape gets its
/// own handler, up to `control.max_clients` at once; one beyond that is
/// closed at once, so a stalled scraper cannot hold the others up.
pub fn serve(
    listener: &TcpListener,
    config: &hearth_core::config::Control,
    servers: &std::sync::Arc<dyn Fn() -> Vec<Value> + Send + Sync>,
) {
    let (control, servers) = (config.clone(), servers.clone());
    crate::listen::serve(
        "metrics",
        listener,
        config.max_clients,
        Duration::from_millis(config.timeout_ms),
        &(std::sync::Arc::new(move |stream| answer(stream, &control, &*servers)) as _),
        drop,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_is_a_series_and_labels_are_escaped() {
        let servers = [serde_json::json!({ "name": "chat\"x", "state": "ready", "restarts": 2 })];
        let text = render(&servers);
        assert!(text.contains("hearth_server_state{server=\"chat\\\"x\",state=\"ready\"} 1"));
        assert!(text.contains("hearth_server_state{server=\"chat\\\"x\",state=\"stopped\"} 0"));
        assert!(text.contains("hearth_server_restarts_total{server=\"chat\\\"x\"} 2"));
        let died = [
            serde_json::json!({ "name": "v", "state": "gave_up", "restarts": 0,
            "last_event": { "event": "exited", "cause": "bad_argument", "decision": "futile" } }),
        ];
        let text = render(&died);
        assert!(text.contains("hearth_server_exit_cause{server=\"v\",cause=\"bad_argument\"} 1"));
        assert!(text.contains("hearth_server_exit_cause{server=\"v\",cause=\"out_of_memory\"} 0"));
        assert!(text.contains("# TYPE hearth_memory_available_bytes gauge"));
        // Each family is declared once.
        assert_eq!(text.matches("# TYPE hearth_server_state ").count(), 1);
    }
}
