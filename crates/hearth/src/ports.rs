//! `hearth ports`: every listening TCP port this user can see, the process
//! behind it, and which model runtime (if any) answers there.
//!
//! Probes are read-only HTTP GETs of the runtimes' status endpoints. Every
//! run is recorded in hearth's journal; the recorded answer times are what
//! later runs learn each runtime's probe deadline from, and what `hearth diff`
//! compares.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use hearth_core::state::Journal;
use hearth_runtime::fingerprint::{Fingerprint, Identity, fingerprint, fingerprint_within};
use hearth_sense::{ProcessFootprint, TcpState};
use serde::{Deserialize as _, Serialize};
use serde_json::Value;

use crate::history::Change;
use crate::units::bytes;

mod findings;
mod journal;

use findings::{findings, oversized, partially_offloaded};
use journal::{NO_ANSWER, recorded};
pub use journal::{diff, entries, summary};

#[derive(clap::Args)]
pub struct Args {
    /// Only ports where a model runtime answers.
    #[arg(long)]
    runtimes: bool,
}

/// The journal kind `ports` records under.
pub const KIND: &str = "ports";

#[derive(Serialize)]
struct Port {
    pid: i32,
    process: String,
    /// Executable path when readable. Learned deadlines and diffs follow a
    /// program by path and port, since its pid changes when it restarts.
    path: Option<String>,
    /// Addresses the process bound (several when it listens on v4 and v6).
    bound: Vec<SocketAddr>,
    #[serde(flatten)]
    fingerprint: Fingerprint,
    memory: Option<ProcessFootprint>,
    /// Accelerator memory the process holds apart from `memory`: a discrete
    /// GPU's (NVIDIA, or AMD and Intel through DRM). Per process, the same
    /// on each of its ports, and an upper bound (shared buffers count in
    /// every holder): never summed across ports or processes. Absent where
    /// the GPU shares host memory (Apple silicon), since the footprint
    /// already counts it.
    #[serde(skip_serializing_if = "Option::is_none")]
    accelerator_bytes: Option<u64>,
    deadline: Deadline,
    /// Missed its learned deadline, then answered within the first-contact
    /// one: busy, swapping, or on its way to wedging.
    slow: bool,
    /// The hearthd server this is, when hearthd started it: the only ports
    /// whose process `restart`, `stop` and `start` may touch. `None` means
    /// someone else runs it, and hearth only observes it.
    owned_by: Option<String>,
}

/// How long the probe waited for this port, and where that number came from.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum Deadline {
    /// `discovery.first_contact_timeout_ms`: fewer recorded answers than
    /// `discovery.learned_min_samples`.
    FirstContact { us: u64, samples: usize },
    /// The slowest recorded answer times `discovery.learned_deadline_multiplier`.
    Learned {
        us: u64,
        samples: usize,
        slowest_us: u64,
    },
}

impl Deadline {
    fn duration(self) -> Duration {
        match self {
            Self::FirstContact { us, .. } | Self::Learned { us, .. } => Duration::from_micros(us),
        }
    }
}

#[derive(Serialize)]
struct Report {
    ports: Vec<Port>,
    /// Processes owned by other users, whose sockets cannot be listed.
    uninspectable_processes: u64,
    first_contact_timeout_ms: u64,
    findings: Vec<Finding>,
    /// Journal id this report was recorded under.
    snapshot: Option<String>,
    /// Why the journal could not be read or written. The report itself is
    /// still complete; only learning and history are affected.
    state_errors: Vec<String>,
}

#[derive(Serialize)]
struct Finding {
    severity: &'static str,
    /// What kind of finding, stable across runs; with `pid`, how `hearth
    /// watch` tells a new problem from one it already reported.
    code: &'static str,
    pid: i32,
    /// The model it is about, when one process serves several.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
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

/// Process name, executable path and bound addresses of one `(pid, port)`.
type Listener = (String, Option<String>, Vec<SocketAddr>);
type Listeners = BTreeMap<(i32, u16), Listener>;

/// `(program, port)`: what a learned deadline belongs to.
type Key = (String, u16);

fn program(path: Option<&str>, process: &str) -> String {
    path.unwrap_or(process).to_string()
}

/// Answer times per program and port, from the recorded `ports` snapshots.
/// Read loosely, field by field, so snapshots from other hearth versions
/// still count.
fn recorded_latencies(journal: &Journal, errors: &mut Vec<String>) -> BTreeMap<Key, Vec<u64>> {
    let mut samples: BTreeMap<Key, Vec<u64>> = BTreeMap::new();
    let snapshots = match journal.recent(KIND, usize::MAX) {
        Ok(s) => s,
        Err(e) => {
            errors.push(e.to_string());
            return samples;
        }
    };
    for snapshot in snapshots {
        let body = match snapshot {
            Ok(s) => s.body,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        for port in body["ports"].as_array().into_iter().flatten() {
            let (Some(us), Some(addr)) = (
                port["latency_us"].as_u64(),
                port["addr"]
                    .as_str()
                    .and_then(|a| a.parse::<SocketAddr>().ok()),
            ) else {
                continue;
            };
            let program = program(
                port["path"].as_str(),
                port["process"].as_str().unwrap_or(""),
            );
            samples.entry((program, addr.port())).or_default().push(us);
        }
    }
    samples
}

fn deadline(samples: Option<&Vec<u64>>, discovery: &hearth_core::config::Discovery) -> Deadline {
    let first_us =
        u64::try_from(Duration::from_millis(discovery.first_contact_timeout_ms).as_micros())
            .unwrap_or(u64::MAX);
    let floor_us =
        u64::try_from(Duration::from_millis(discovery.learned_deadline_floor_ms).as_micros())
            .unwrap_or(u64::MAX)
            .max(1);
    let n = samples.map_or(0, Vec::len);
    match samples.and_then(|s| s.iter().max()) {
        Some(&slowest_us) if n >= discovery.learned_min_samples => {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let learned = ((slowest_us as f64 * discovery.learned_deadline_multiplier).ceil()
                as u64)
                .max(floor_us);
            if learned >= first_us {
                Deadline::FirstContact {
                    us: first_us,
                    samples: n,
                }
            } else {
                Deadline::Learned {
                    us: learned,
                    samples: n,
                    slowest_us,
                }
            }
        }
        _ => Deadline::FirstContact {
            us: first_us,
            samples: n,
        },
    }
}

/// Probes `addr`, giving a runtime that misses its learned deadline a second
/// chance with the first-contact one. Returns whether it needed that.
fn probe(
    addr: SocketAddr,
    deadline: Deadline,
    first: Deadline,
    max_bytes: u64,
) -> (Fingerprint, bool) {
    // The learned deadline was learned on the first request only, so it
    // bounds that one; the requests after it keep the first-contact deadline.
    let fp = fingerprint_within(addr, deadline.duration(), first.duration(), max_bytes);
    match (deadline, &fp.identity) {
        (Deadline::Learned { .. }, Identity::Unresponsive { waited_ms }) => {
            let mut again = fingerprint(addr, first.duration(), max_bytes);
            let answered = !matches!(again.identity, Identity::Unresponsive { .. });
            if let Identity::Unresponsive { waited_ms: second } = &mut again.identity {
                *second += waited_ms;
            }
            (again, answered)
        }
        _ => (fp, false),
    }
}

/// Every listening socket this user can see, by `(pid, port)`, and how many
/// processes could not be inspected.
fn listeners() -> Result<(Listeners, u64), String> {
    let mut listeners = Listeners::new();
    let mut uninspectable = 0;
    for p in hearth_sense::list_processes().map_err(|e| e.to_string())? {
        let Ok(sockets) = hearth_sense::tcp_sockets(p.pid) else {
            uninspectable += 1;
            continue;
        };
        for s in sockets.into_iter().filter(|s| s.state == TcpState::Listen) {
            listeners
                .entry((p.pid, s.local.port()))
                .or_insert_with(|| (p.name.clone(), p.path.clone(), Vec::new()))
                .2
                .push(s.local);
        }
    }
    Ok((listeners, uninspectable))
}

type Probed = BTreeMap<(i32, u16), (Fingerprint, Deadline, bool)>;

/// Probes in parallel, one worker per core: every probe may have to wait out
/// its whole deadline on a server that never answers.
fn probe_all(
    jobs: Vec<((i32, u16), SocketAddr, Deadline)>,
    first: Deadline,
    max_bytes: u64,
) -> Result<Probed, String> {
    let workers = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let queue = Mutex::new(jobs.into_iter());
    let results = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let next = queue.lock().ok().and_then(|mut q| q.next());
                    let Some((key, addr, deadline)) = next else {
                        break;
                    };
                    let (fp, slow) = probe(addr, deadline, first, max_bytes);
                    if let Ok(mut r) = results.lock() {
                        r.insert(key, (fp, deadline, slow));
                    }
                }
            });
        }
    });
    results.into_inner().map_err(|e| e.to_string())
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let report = observe(args.runtimes)?;
    if json {
        crate::to_json(&report)
    } else {
        Ok(render(&report))
    }
}

/// Probes every listening port, and records the report unless it was
/// filtered to runtimes.
/// Server pids hearthd keeps, by pid, with each server's name; empty when
/// hearthd is not running.
fn owned_pids() -> BTreeMap<i32, String> {
    #[cfg(unix)]
    if let Ok(answer) = crate::control::call(&serde_json::json!({ "op": "status" })) {
        return answer["servers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let pid = i32::try_from(s["server_pid"].as_i64()?).ok()?;
                Some((pid, s["name"].as_str()?.to_string()))
            })
            .collect();
    }
    BTreeMap::new()
}

fn observe(runtimes_only: bool) -> Result<Report, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let owned = owned_pids();
    let discovery = &config.discovery;
    let max_bytes = discovery.max_probe_response_bytes;
    let mut state_errors = Vec::new();
    let journal = Journal::open(&config.state)
        .map_err(|e| state_errors.push(e.to_string()))
        .ok();
    let learned = journal
        .as_ref()
        .map(|j| recorded_latencies(j, &mut state_errors))
        .unwrap_or_default();
    let (listeners, uninspectable) = listeners()?;

    // Probe each port once, preferring IPv4.
    let jobs: Vec<((i32, u16), SocketAddr, Deadline)> = listeners
        .iter()
        .map(|(key, (process, path, bound))| {
            let pick = bound.iter().find(|a| a.is_ipv4()).unwrap_or(&bound[0]);
            let samples = learned.get(&(program(path.as_deref(), process), key.1));
            (*key, reachable(*pick), deadline(samples, discovery))
        })
        .collect();
    let results = probe_all(jobs, deadline(None, discovery), max_bytes)?;

    // Asked once, for the processes where a runtime answered (a process on
    // several ports once).
    let mut runtime_pids: Vec<i32> = results
        .iter()
        .filter(|(_, (f, _, _))| is_runtime(&f.identity))
        .map(|((pid, _), _)| *pid)
        .collect();
    runtime_pids.sort_unstable();
    runtime_pids.dedup();
    let on_accelerator = if runtime_pids.is_empty() {
        Vec::new()
    } else {
        match crate::pressure::accelerator_process_bytes(&runtime_pids) {
            Ok((found, errors)) => {
                state_errors.extend(
                    errors
                        .into_iter()
                        .map(|e| format!("accelerator memory per process: {e}")),
                );
                found
            }
            Err(e) => {
                state_errors.push(format!("accelerator memory per process: {e}"));
                Vec::new()
            }
        }
    };
    let ports: Vec<Port> = listeners
        .into_iter()
        .filter_map(|(key, (process, path, bound))| {
            let (fingerprint, deadline, slow) = results.get(&key)?.clone();
            let is_runtime = is_runtime(&fingerprint.identity);
            if runtimes_only && !is_runtime {
                return None;
            }
            Some(Port {
                owned_by: owned.get(&key.0).cloned(),
                accelerator_bytes: on_accelerator
                    .iter()
                    .find(|(pid, _)| *pid == key.0)
                    .map(|(_, b)| *b),
                pid: key.0,
                memory: is_runtime
                    .then(|| hearth_sense::process_footprint(key.0).ok())
                    .flatten(),
                process,
                path,
                bound,
                fingerprint,
                deadline,
                slow,
            })
        })
        .collect();
    let mut findings = findings(&ports);
    findings.extend(oversized(&ports));
    findings.extend(partially_offloaded(&ports, || {
        crate::pressure::accelerators().is_ok_and(|a| !a.is_empty())
    }));
    let mut report = Report {
        ports,
        uninspectable_processes: uninspectable,
        first_contact_timeout_ms: discovery.first_contact_timeout_ms,
        findings,
        snapshot: None,
        state_errors,
    };
    // A filtered run is not a full picture of the machine, so it is not
    // recorded: a later diff would report every other port as vanished.
    if !runtimes_only && let Some(journal) = &journal {
        match journal.record(KIND, &report) {
            Ok(r) => {
                report.snapshot = Some(r.snapshot.id);
                report
                    .state_errors
                    .extend(r.warnings.iter().map(ToString::to_string));
            }
            Err(e) => report.state_errors.push(e.to_string()),
        }
    }
    Ok(report)
}

/// A full, recorded `ports` report as JSON, the form `diff` compares.
pub fn observe_recorded() -> Result<(Value, Option<String>, Vec<String>), String> {
    let report = observe(false)?;
    let value = serde_json::to_value(&report).map_err(|e| e.to_string())?;
    Ok((value, report.snapshot, report.state_errors))
}

/// `(program, port)` of every port in `body` where a model runtime answers.
pub fn runtime_keys(body: &Value) -> std::collections::BTreeSet<(String, u16)> {
    recorded(body)
        .into_iter()
        .filter(|(_, by_pid)| {
            by_pid
                .values()
                .any(|p| Identity::deserialize(*p).is_ok_and(|i| is_runtime(&i)))
        })
        .map(|(k, _)| k)
        .collect()
}

/// Changes a person should hear about between two reports, among ports
/// where a runtime answered at any time `known` covers (a watcher passes
/// every one it has seen, so a runtime that went silent and then vanished is
/// still known): a runtime that vanished, restarted, stopped answering, or
/// answered again. Other listeners come and go all the time and are left
/// out.
pub fn alarms(
    before: &Value,
    after: &Value,
    known: &std::collections::BTreeSet<(String, u16)>,
) -> Vec<Change> {
    let keys: std::collections::BTreeSet<Key> = runtime_keys(before)
        .union(&runtime_keys(after))
        .chain(known)
        .cloned()
        .collect();
    let only = |body: &Value| {
        let ports: Vec<Value> = body["ports"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| {
                let port = p["addr"]
                    .as_str()
                    .and_then(|a| a.parse::<SocketAddr>().ok())
                    .map(|a| a.port());
                port.is_some_and(|port| {
                    let program = program(p["path"].as_str(), p["process"].as_str().unwrap_or(""));
                    keys.contains(&(program, port))
                })
            })
            .cloned()
            .collect();
        serde_json::json!({ "ports": ports })
    };
    diff(&only(before), &only(after))
        .0
        .into_iter()
        .filter(|c| match c.kind {
            "vanished" | "restarted" => true,
            "changed" => [&c.before, &c.after]
                .iter()
                .any(|side| side.as_deref().is_some_and(|s| s.contains(NO_ANSWER))),
            _ => false,
        })
        .collect()
}

fn is_runtime(identity: &Identity) -> bool {
    identity.is_runtime()
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
                    .map(|l| {
                        format!(
                            "{} ({}{})",
                            l.name,
                            l.size_bytes.map_or("?".into(), bytes),
                            l.context_length
                                .map_or(String::new(), |c| format!(", ctx {c}"))
                        )
                    })
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
            tokens_held,
        } => format!(
            "llama.cpp · {} · ctx {}{} · slots {} · health {health}{}",
            model.as_deref().unwrap_or("?"),
            n_ctx.map_or("?".into(), |n| n.to_string()),
            tokens_held.map_or(String::new(), |t| format!(" ({t} held)")),
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

fn explain(d: Deadline) -> String {
    let t = Duration::from_micros;
    match d {
        Deadline::FirstContact { us, .. } => format!("first-contact deadline {:?}", t(us)),
        Deadline::Learned {
            us,
            samples,
            slowest_us,
        } => format!(
            "{:?}, from the slowest of {samples} recorded answers ({:?})",
            t(us),
            t(slowest_us)
        ),
    }
}

fn render(r: &Report) -> String {
    let mut out = String::new();
    for p in &r.ports {
        let memory = p.memory.as_ref().map_or(String::new(), |m| {
            format!(" · footprint {}", bytes(m.footprint_bytes))
        }) + &p.accelerator_bytes.map_or(String::new(), |b| {
            format!(" + {} on the accelerator", bytes(b))
        });
        let _ = writeln!(
            out,
            "{:<22} {} ({}){} · {}{memory}",
            p.fingerprint.addr,
            p.process,
            p.pid,
            p.owned_by
                .as_ref()
                .map_or(String::new(), |n| format!(" [hearthd: {n}]")),
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
    out + &crate::history::footer(r.snapshot.as_deref(), &r.state_errors)
}

#[cfg(test)]
mod tests;
