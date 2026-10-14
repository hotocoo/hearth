//! `hearth ports`: every listening TCP port this user can see, the process
//! behind it, and which model runtime (if any) answers there.
//!
//! Probes are read-only HTTP GETs of the runtimes' status endpoints.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use hearth_runtime::fingerprint::{Fingerprint, Identity, fingerprint};
use hearth_sense::{ProcessFootprint, TcpState};
use serde::Serialize;

use crate::units::bytes;

#[derive(clap::Args)]
pub struct Args {
    /// Only ports where a model runtime answers.
    #[arg(long)]
    runtimes: bool,
}

#[derive(Serialize)]
struct Port {
    pid: i32,
    process: String,
    /// Addresses the process bound (several when it listens on v4 and v6).
    bound: Vec<SocketAddr>,
    #[serde(flatten)]
    fingerprint: Fingerprint,
    memory: Option<ProcessFootprint>,
}

#[derive(Serialize)]
struct Report {
    ports: Vec<Port>,
    /// Processes owned by other users, whose sockets cannot be listed.
    uninspectable_processes: u64,
    probe_deadline_ms: u64,
    findings: Vec<Finding>,
}

#[derive(Serialize)]
struct Finding {
    severity: &'static str,
    message: String,
    remediation: String,
}

/// Where to connect to reach a socket bound to `addr` from this machine.
fn reachable(addr: SocketAddr) -> SocketAddr {
    let ip = match addr.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    SocketAddr::new(ip, addr.port())
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let deadline = Duration::from_millis(config.discovery.first_contact_timeout_ms);
    let max_bytes = config.discovery.max_probe_response_bytes;

    // (pid, port) -> (process name, bound addresses)
    let mut listeners: BTreeMap<(i32, u16), (String, Vec<SocketAddr>)> = BTreeMap::new();
    let mut uninspectable = 0;
    for p in hearth_sense::list_processes().map_err(|e| e.to_string())? {
        let Ok(sockets) = hearth_sense::tcp_sockets(p.pid) else {
            uninspectable += 1;
            continue;
        };
        for s in sockets.into_iter().filter(|s| s.state == TcpState::Listen) {
            listeners
                .entry((p.pid, s.local.port()))
                .or_insert_with(|| (p.name.clone(), Vec::new()))
                .1
                .push(s.local);
        }
    }

    // Probe each port once, preferring IPv4, in parallel: every probe may
    // have to wait out the full deadline on a server that never answers.
    let jobs: Vec<((i32, u16), SocketAddr)> = listeners
        .iter()
        .map(|(key, (_, bound))| {
            let pick = bound.iter().find(|a| a.is_ipv4()).unwrap_or(&bound[0]);
            (*key, reachable(*pick))
        })
        .collect();
    let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let queue = Mutex::new(jobs.into_iter());
    let results = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().ok().and_then(|mut q| q.next());
                    let Some((key, addr)) = next else { break };
                    let fp = fingerprint(addr, deadline, max_bytes);
                    if let Ok(mut r) = results.lock() {
                        r.insert(key, fp);
                    }
                }
            });
        }
    });
    let results = results.into_inner().map_err(|e| e.to_string())?;

    let ports: Vec<Port> = listeners
        .into_iter()
        .filter_map(|(key, (process, bound))| {
            let fingerprint = results.get(&key)?.clone();
            let is_runtime = is_runtime(&fingerprint.identity);
            if args.runtimes && !is_runtime {
                return None;
            }
            Some(Port {
                pid: key.0,
                memory: is_runtime
                    .then(|| hearth_sense::process_footprint(key.0).ok())
                    .flatten(),
                process,
                bound,
                fingerprint,
            })
        })
        .collect();
    let findings = findings(&ports);
    let report = Report {
        ports,
        uninspectable_processes: uninspectable,
        probe_deadline_ms: config.discovery.first_contact_timeout_ms,
        findings,
    };
    if json {
        crate::to_json(&report)
    } else {
        Ok(render(&report))
    }
}

fn is_runtime(identity: &Identity) -> bool {
    matches!(
        identity,
        Identity::Ollama { .. }
            | Identity::LlamaCpp { .. }
            | Identity::ComfyUi { .. }
            | Identity::StableDiffusionApi { .. }
            | Identity::LmStudio { .. }
            | Identity::OpenAiCompatible { .. }
    )
}

fn findings(ports: &[Port]) -> Vec<Finding> {
    // Silence from a process that serves a model on another port is a likely
    // wedge; silence from anything else may just be a non-HTTP protocol.
    let runtime_pids: std::collections::BTreeSet<i32> = ports
        .iter()
        .filter(|p| is_runtime(&p.fingerprint.identity))
        .map(|p| p.pid)
        .collect();
    let mut out = Vec::new();
    for p in ports {
        match &p.fingerprint.identity {
            Identity::Unresponsive { waited_ms } => out.push(Finding {
                severity: if runtime_pids.contains(&p.pid) {
                    "warn"
                } else {
                    "info"
                },
                message: format!(
                    "{} (pid {}) accepts connections on {} but did not answer within {waited_ms} ms",
                    p.process, p.pid, p.fingerprint.addr
                ),
                remediation: format!(
                    "if {} is a model server, it may be wedged: check its log; restart it if it stays silent",
                    p.process
                ),
            }),
            Identity::LlamaCpp { health, .. } if health != "ok" => out.push(Finding {
                severity: "info",
                message: format!(
                    "llama-server on {} is not ready: {health}",
                    p.fingerprint.addr
                ),
                remediation: "a 503 while loading is normal; if it persists, read the server log"
                    .into(),
            }),
            _ => {}
        }
    }
    out
}

fn describe(identity: &Identity) -> String {
    let list = |items: &[String]| {
        if items.is_empty() {
            "no models".to_string()
        } else {
            items.join(", ")
        }
    };
    match identity {
        Identity::Ollama {
            version,
            models,
            loaded,
        } => format!(
            "Ollama {version} · {} models · loaded: {}",
            models.len(),
            if loaded.is_empty() {
                "none".to_string()
            } else {
                loaded
                    .iter()
                    .map(|l| format!("{} ({})", l.name, l.size_bytes.map_or("?".into(), bytes)))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
        Identity::LlamaCpp {
            model,
            n_ctx,
            slots,
            health,
            models,
        } => format!(
            "llama.cpp · {} · ctx {} · slots {} · health {health}{}",
            model.as_deref().unwrap_or("?"),
            n_ctx.map_or("?".into(), |n| n.to_string()),
            slots.map_or("?".into(), |n| n.to_string()),
            if models.len() > 1 {
                format!(" · router: {}", list(models))
            } else {
                String::new()
            }
        ),
        Identity::ComfyUi { version, devices } => format!(
            "ComfyUI {} · {}",
            version.as_deref().unwrap_or(""),
            list(devices)
        ),
        Identity::StableDiffusionApi { models } => format!("SD WebUI API · {}", list(models)),
        Identity::LmStudio { models } => format!(
            "LM Studio · {}",
            models
                .iter()
                .map(|(m, s)| format!("{m} ({s})"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Identity::OpenAiCompatible { models } => format!("OpenAI-compatible · {}", list(models)),
        Identity::OtherHttp { status } => format!("other HTTP service ({status})"),
        Identity::NotHttp => "not HTTP".into(),
        Identity::Unresponsive { waited_ms } => format!("NO ANSWER after {waited_ms} ms"),
        Identity::Refused => "refused".into(),
        Identity::Error { detail } => format!("error: {detail}"),
    }
}

fn render(r: &Report) -> String {
    let mut out = String::new();
    for p in &r.ports {
        let memory = p.memory.as_ref().map_or(String::new(), |m| {
            format!(" · footprint {}", bytes(m.footprint_bytes))
        });
        let _ = writeln!(
            out,
            "{:<22} {} ({}) · {}{memory}",
            p.fingerprint.addr,
            p.process,
            p.pid,
            describe(&p.fingerprint.identity)
        );
    }
    if r.uninspectable_processes > 0 {
        let _ = writeln!(
            out,
            "\n{} processes owned by other users were not inspected",
            r.uninspectable_processes
        );
    }
    for f in &r.findings {
        let _ = writeln!(
            out,
            "\n[{}] {}\n  → {}",
            f.severity, f.message, f.remediation
        );
    }
    out
}
