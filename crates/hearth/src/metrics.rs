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
    let gpus: Vec<hearth_sense::Accelerator> = hearth_sense::accelerators()
        .unwrap_or_default()
        .into_iter()
        .filter(|a| !a.unified_memory)
        .collect();
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
            for a in &gpus {
                if let Some(v) = pick(a) {
                    let _ = writeln!(out, "{name}{{gpu=\"{}\"}} {v}", label(&a.name));
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

/// The request head (up to the blank line), read within `max` bytes and
/// before `deadline` in total: each read waits at most the time left when
/// reading began, and the deadline is checked after every byte, so a client
/// sending a byte at a time cannot hold the handler.
fn read_head(stream: &TcpStream, max: u64, deadline: Instant) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read as _};
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| Error::new(ErrorKind::TimedOut, "request took too long"))
    };
    stream.set_read_timeout(Some(left()?))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    let mut limited = stream.take(max);
    while !head.ends_with(b"\r\n\r\n") && !head.ends_with(b"\n\n") {
        if limited.read(&mut byte)? == 0 {
            break;
        }
        head.push(byte[0]);
        left()?;
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Answers one scrape. Anything but `GET /metrics` is a 404.
fn answer(
    mut stream: TcpStream,
    config: &hearth_core::config::Control,
    servers: &dyn Fn() -> Vec<Value>,
) -> std::io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
    let head = read_head(&stream, config.max_message_bytes, deadline)?;
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
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Serves metrics on `listener` until hearthd ends. Each scrape gets its
/// own handler, up to `control.max_clients` at once; one beyond that is
/// closed at once, so a stalled scraper cannot hold the others up.
pub fn serve(
    listener: &TcpListener,
    config: &hearth_core::config::Control,
    servers: &std::sync::Arc<dyn Fn() -> Vec<Value> + Send + Sync>,
) {
    use std::sync::atomic::{AtomicU64, Ordering};
    let busy = std::sync::Arc::new(AtomicU64::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(std::io::stdout(), "metrics: {e}");
                std::thread::sleep(Duration::from_millis(config.timeout_ms));
                continue;
            }
        };
        if busy.fetch_add(1, Ordering::SeqCst) >= config.max_clients {
            busy.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let (config, servers, done) = (config.clone(), servers.clone(), busy.clone());
        let spawned = std::thread::Builder::new()
            .name("metrics scrape".into())
            .spawn(move || {
                if let Err(e) = answer(stream, &config, &*servers) {
                    let _ = writeln!(std::io::stdout(), "metrics: {e}");
                }
                done.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            busy.fetch_sub(1, Ordering::SeqCst);
            let _ = writeln!(std::io::stdout(), "metrics: no thread for a scrape: {e}");
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
