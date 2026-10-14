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

use crate::history::{Change, Delta, Unit};
use crate::units::bytes;

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
    let findings = findings(&ports);
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
        if let Identity::Unresponsive { waited_ms } = &p.fingerprint.identity {
            out.push(Finding {
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
            });
        }
        if p.slow {
            out.push(Finding {
                severity: "warn",
                message: format!(
                    "{} (pid {}) on {} answered only after its learned deadline ({})",
                    p.process,
                    p.pid,
                    p.fingerprint.addr,
                    explain(p.deadline)
                ),
                remediation: format!(
                    "{} is much slower than it has been: check memory pressure (hearth doctor) and its log; a server that keeps slowing may be about to wedge",
                    p.process
                ),
            });
        }
        match &p.fingerprint.identity {
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

/// The ports in a recorded report, keyed by program and port, each with
/// every pid listening there (prefork servers and `SO_REUSEPORT` share one).
/// Read field by field so snapshots from other hearth versions still compare.
fn recorded(body: &Value) -> BTreeMap<Key, BTreeMap<i64, &Value>> {
    let mut out: BTreeMap<Key, BTreeMap<i64, &Value>> = BTreeMap::new();
    for p in body["ports"].as_array().into_iter().flatten() {
        let Some(port) = p["addr"]
            .as_str()
            .and_then(|a| a.parse::<SocketAddr>().ok())
            .map(|a| a.port())
        else {
            continue;
        };
        let program = program(p["path"].as_str(), p["process"].as_str().unwrap_or(""));
        out.entry((program, port))
            .or_default()
            .insert(p["pid"].as_i64().unwrap_or_default(), p);
    }
    out
}

/// How a port that accepts connections and never answers is described.
const NO_ANSWER: &str = "NO ANSWER";

/// What a recorded port looked like, without the parts that differ on every
/// probe (how long a silent port was waited for).
fn state_of(port: &Value) -> String {
    match Identity::deserialize(port) {
        Ok(Identity::Unresponsive { .. }) => NO_ANSWER.into(),
        Ok(identity) => describe(&identity),
        Err(_) => port["kind"].as_str().unwrap_or("unknown").to_string(),
    }
}

/// Each port in a recorded report as `(who, how it was)`: who is process,
/// pid, address and path, for `hearth follow` to match; how is the state
/// with the footprint when one was measured.
pub fn entries(body: &Value) -> Vec<(String, String)> {
    body["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let who = format!(
                "{} pid {} {} {}",
                p["process"].as_str().unwrap_or("?"),
                p["pid"],
                p["addr"].as_str().unwrap_or("?"),
                p["path"].as_str().unwrap_or("")
            );
            let memory = p["memory"]["footprint_bytes"]
                .as_u64()
                .map_or(String::new(), |b| format!(" · footprint {}", bytes(b)));
            (
                who.trim_end().to_string(),
                format!("{}{memory}", state_of(p)),
            )
        })
        .collect()
}

/// One line for `hearth log`.
pub fn summary(body: &Value) -> String {
    let ports = recorded(body);
    let listening: usize = ports.values().map(BTreeMap::len).sum();
    let runtimes: Vec<String> = ports
        .iter()
        .filter_map(|((_, port), by_pid)| {
            let p = by_pid
                .values()
                .find(|p| Identity::deserialize(**p).is_ok_and(|i| is_runtime(&i)))?;
            Some(format!("{}:{port}", p["process"].as_str().unwrap_or("?")))
        })
        .collect();
    let findings = body["findings"].as_array().map_or(0, Vec::len);
    format!(
        "{listening} listening, runtimes: {}, {findings} findings",
        if runtimes.is_empty() {
            "none".into()
        } else {
            runtimes.join(" ")
        }
    )
}

fn pids(by_pid: &BTreeMap<i64, &Value>) -> String {
    let list: Vec<String> = by_pid.keys().map(ToString::to_string).collect();
    format!("pid {}", list.join(","))
}

/// The distinct states of the processes on one port, in pid order.
fn states(by_pid: &BTreeMap<i64, &Value>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for state in by_pid.values().map(|p| state_of(p)) {
        if !seen.contains(&state) {
            seen.push(state);
        }
    }
    seen.join(" | ")
}

/// What appeared, vanished, restarted or changed between two recorded
/// reports, and how runtime footprints moved.
pub fn diff(before: &Value, after: &Value) -> (Vec<Change>, Vec<Delta>) {
    let (before, after) = (recorded(before), recorded(after));
    let subject = |(program, port): &Key, by_pid: &BTreeMap<i64, &Value>| {
        let name = by_pid
            .values()
            .find_map(|p| p["process"].as_str().filter(|n| !n.is_empty()))
            .unwrap_or(program);
        format!("{name} :{port}")
    };
    let side = |by_pid: &BTreeMap<i64, &Value>| format!("{} · {}", pids(by_pid), states(by_pid));
    let mut changes = Vec::new();
    let mut deltas = Vec::new();
    for (key, b) in &before {
        if !after.contains_key(key) {
            changes.push(Change {
                kind: "vanished",
                subject: subject(key, b),
                before: Some(side(b)),
                after: None,
            });
        }
    }
    for (key, a) in &after {
        let Some(b) = before.get(key) else {
            changes.push(Change {
                kind: "appeared",
                subject: subject(key, a),
                before: None,
                after: Some(side(a)),
            });
            continue;
        };
        if !b.keys().eq(a.keys()) {
            changes.push(Change {
                kind: "restarted",
                subject: subject(key, a),
                before: Some(side(b)),
                after: Some(side(a)),
            });
        } else if states(b) != states(a) {
            changes.push(Change {
                kind: "changed",
                subject: subject(key, a),
                before: Some(states(b)),
                after: Some(states(a)),
            });
        }
        // Footprints compare only for a process that survived.
        for (pid, p) in a {
            if let Some(old) = b.get(pid)
                && let (Some(x), Some(y)) = (
                    old["memory"]["footprint_bytes"].as_u64(),
                    p["memory"]["footprint_bytes"].as_u64(),
                )
            {
                let who = if a.len() > 1 {
                    format!("{} pid {pid}", subject(key, a))
                } else {
                    subject(key, a)
                };
                deltas.push(Delta::new(who, "footprint", Unit::Bytes, x, y));
            }
        }
    }
    (changes, deltas)
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
mod tests {
    use super::*;
    use serde_json::json;

    fn port(pid: i32, port: u16, identity: &Value, footprint: Option<u64>) -> Value {
        let mut p = json!({
            "pid": pid, "process": "llama-server", "path": "/opt/bin/llama-server",
            "addr": format!("127.0.0.1:{port}"),
            "memory": footprint.map(|f| json!({ "footprint_bytes": f })),
        });
        for (k, v) in identity.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    }

    #[test]
    fn diff_reports_restarts_changes_and_footprint() {
        let ready = json!({"kind":"llama_cpp","model":"/m/a.gguf","n_ctx":4096,"slots":1,"health":"ok","models":[]});
        let silent = json!({"kind":"unresponsive","waited_ms":2500});
        let silent_longer = json!({"kind":"unresponsive","waited_ms":2501});
        let before = json!({"ports": [
            port(10, 8080, &ready, Some(100)),
            port(11, 8081, &ready, None),
            port(12, 8082, &silent, None),
            port(13, 9000, &ready, None),
        ]});
        let after = json!({"ports": [
            port(10, 8080, &ready, Some(300)),
            port(21, 8081, &ready, None),
            port(12, 8082, &silent_longer, None),
            port(14, 9001, &silent, None),
        ]});
        let (changes, deltas) = diff(&before, &after);
        let kinds: Vec<(&str, &str)> = changes
            .iter()
            .map(|c| (c.kind, c.subject.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("vanished", "llama-server :9000"),
                ("restarted", "llama-server :8081"),
                ("appeared", "llama-server :9001"),
            ],
            "a longer wait on a port that was already silent is not a change"
        );
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].by, 200);
        let (none, _) = diff(&after, &after);
        assert!(none.is_empty());
    }

    #[test]
    fn workers_sharing_a_port_are_compared_as_a_set() {
        let ready = json!({"kind":"open_ai_compatible","models":["m"]});
        let two = |a: i32, b: i32| json!({"ports": [port(a, 8000, &ready, Some(10)), port(b, 8000, &ready, Some(10))]});
        let (none, _) = diff(&two(1, 2), &two(1, 2));
        assert!(none.is_empty());
        assert_eq!(summary(&two(1, 2)).split(' ').next(), Some("2"));
        let (changes, deltas) = diff(&two(1, 2), &two(1, 3));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "restarted");
        assert_eq!(
            changes[0].after.as_deref(),
            Some("pid 1,3 · OpenAI-compatible · m")
        );
        assert_eq!(
            deltas.len(),
            1,
            "only the surviving worker's footprint compares"
        );
    }

    #[test]
    fn alarms_are_runtimes_that_vanished_restarted_or_went_silent() {
        let ready = json!({"kind":"ollama","version":"0.30","models":[],"loaded":[]});
        let silent = json!({"kind":"unresponsive","waited_ms":2500});
        let other = json!({"kind":"other_http","status":404});
        let mut dev = port(30, 3000, &other, None);
        dev["path"] = json!("/usr/bin/node");
        let before =
            json!({"ports": [port(5, 11434, &ready, None), port(6, 8080, &ready, None), dev]});
        let after = json!({"ports": [port(5, 11434, &silent, None)]});
        let alarms = alarms(&before, &after, &std::collections::BTreeSet::default());
        let kinds: Vec<&str> = alarms.iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            ["vanished", "changed"],
            "a dev server that went away is not an alarm: {:?}",
            alarms.iter().map(|c| &c.subject).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_known_runtime_that_went_silent_then_vanished_is_still_reported() {
        let ready = json!({"kind":"ollama","version":"0.30","models":[],"loaded":[]});
        let silent = json!({"kind":"unresponsive","waited_ms":2500});
        let first = json!({"ports": [port(5, 11434, &ready, None)]});
        let second = json!({"ports": [port(5, 11434, &silent, None)]});
        let third = json!({"ports": []});
        let known = runtime_keys(&first);
        assert_eq!(alarms(&second, &third, &known)[0].kind, "vanished");
        assert!(alarms(&second, &third, &std::collections::BTreeSet::default()).is_empty());
        // Answering again is reported too.
        assert_eq!(alarms(&second, &first, &known)[0].kind, "changed");
    }

    #[test]
    fn diff_reports_a_runtime_that_stopped_answering() {
        let ready = json!({"kind":"ollama","version":"0.30","models":[],"loaded":[]});
        let silent = json!({"kind":"unresponsive","waited_ms":2500});
        let (changes, _) = diff(
            &json!({"ports": [port(5, 11434, &ready, None)]}),
            &json!({"ports": [port(5, 11434, &silent, None)]}),
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "changed");
        assert_eq!(changes[0].after.as_deref(), Some("NO ANSWER"));
    }

    fn discovery(min_samples: usize) -> hearth_core::config::Discovery {
        let mut d = hearth_core::config::from_layers(None).unwrap().discovery;
        d.learned_min_samples = min_samples;
        d.learned_deadline_floor_ms = 0;
        d
    }

    #[test]
    fn deadline_is_learned_only_from_enough_answers() {
        let d = discovery(3);
        assert!(matches!(
            deadline(Some(&vec![100, 300]), &d),
            Deadline::FirstContact { samples: 2, .. }
        ));
        let Deadline::Learned { us, slowest_us, .. } = deadline(Some(&vec![100, 300, 200]), &d)
        else {
            panic!("three answers are enough")
        };
        assert_eq!(slowest_us, 300);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let expected = (300.0 * d.learned_deadline_multiplier).ceil() as u64;
        assert_eq!(us, expected);
    }

    #[test]
    fn learned_deadline_never_drops_below_the_floor() {
        let mut d = discovery(1);
        d.learned_deadline_floor_ms = 7;
        let Deadline::Learned { us, .. } = deadline(Some(&vec![1]), &d) else {
            panic!("a learned deadline")
        };
        assert_eq!(Duration::from_micros(us), Duration::from_millis(7));
    }

    #[test]
    fn learned_deadline_never_exceeds_first_contact() {
        let d = discovery(1);
        let first = deadline(None, &d);
        let Deadline::FirstContact { us: first_us, .. } = first else {
            panic!()
        };
        assert!(matches!(
            deadline(Some(&vec![first_us]), &d),
            Deadline::FirstContact { .. }
        ));
    }

    #[test]
    fn missed_learned_deadline_gets_a_second_chance() {
        use std::io::{Read, Write};
        // Answers like Ollama, but only after a pause longer than the
        // learned deadline and shorter than the first-contact one.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let pause = Duration::from_millis(50);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                std::thread::sleep(pause);
                let body = r#"{"version":"1"}"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        let learned = Deadline::Learned {
            us: 1000,
            samples: 3,
            slowest_us: 100,
        };
        let first = Deadline::FirstContact {
            us: u64::try_from((pause * 40).as_micros()).unwrap(),
            samples: 0,
        };
        let (fp, slow) = probe(addr, learned, first, 1 << 16);
        assert!(slow);
        assert!(
            matches!(fp.identity, Identity::Ollama { .. }),
            "{:?}",
            fp.identity
        );
    }
}
