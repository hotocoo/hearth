//! Prometheus metrics from hearthd, for Grafana or any scraper: the state
//! of each server hearthd keeps, and the machine's memory and kernel-wide
//! counts, measured at each scrape.
//!
//! Off unless `metrics.listen` is set. The endpoint is read-only and says
//! nothing that is not already in `hearth status` and `hearth doctor`.
//! Labels are bounded: server names from the configuration, and states from
//! a fixed set.

use std::fmt::Write as _;
use std::io::Write as _;
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
    if let Ok(m) = hearth_sense::host_memory() {
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

/// Answers one scrape. Anything but `GET /metrics` is a 404; the request is
/// read within the control limits, like the control socket's.
fn answer(
    mut stream: TcpStream,
    config: &hearth_core::config::Control,
    servers: &dyn Fn() -> Vec<Value>,
) -> std::io::Result<()> {
    use std::io::{BufRead as _, BufReader, Read as _};
    let timeout = Duration::from_millis(config.timeout_ms);
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let started = Instant::now();
    let mut reader = BufReader::new((&stream).take(config.max_message_bytes));
    let mut first = String::new();
    reader.read_line(&mut first)?;
    // The rest of the request head; its content is not needed.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 2 && started.elapsed() < timeout {
        line.clear();
    }
    let (status, body) = if first.starts_with("GET /metrics ") {
        ("200 OK", render(&servers()))
    } else {
        ("404 Not Found", "only GET /metrics is served\n".to_string())
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves metrics on `listener` until hearthd ends, one scrape at a time.
pub fn serve(
    listener: &TcpListener,
    config: &hearth_core::config::Control,
    servers: &dyn Fn() -> Vec<Value>,
) {
    for stream in listener.incoming() {
        let result = stream.and_then(|s| answer(s, config, servers));
        if let Err(e) = result {
            let _ = writeln!(std::io::stdout(), "metrics: {e}");
        }
    }
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
        assert!(text.contains("# TYPE hearth_memory_available_bytes gauge"));
        // Each family is declared once.
        assert_eq!(text.matches("# TYPE hearth_server_state ").count(), 1);
    }
}
