//! `hearth run`: start a model server and keep it alive.
//!
//! hearth owns what it starts here, and only that: the port is asked who is
//! there first, and a port that another process already serves is never
//! touched. The server runs under a tether, so it stops when hearth does,
//! however hearth stops.

use std::ffi::OsString;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::time::{Duration, Instant};

use hearth_core::state::Journal;
use hearth_runtime::supervise::{
    Decision, Event, Preflight, Probe, RestartPolicy, Settings, Supervised, preflight,
};

/// A cache type name `hearth fit` accepts.
fn cache_name(s: &str) -> Result<String, String> {
    crate::fit::parse_cache(s).map(|_| s.to_string())
}

/// The journal kind `run` records its events under.
pub const KIND: &str = "run";

#[derive(clap::Args)]
pub struct Args {
    /// Port the server listens on, which hearth probes.
    #[arg(long)]
    port: u16,
    /// Address to probe the port on.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    host: IpAddr,
    /// The model the server loads (as `hearth fit` takes it), with --ctx
    /// and --cache-type: checked to fit the device before every start.
    #[arg(long, requires_all = ["ctx", "cache_type"])]
    model: Option<String>,
    /// Context the server is given, for the fit check.
    #[arg(long, requires = "model")]
    ctx: Option<u64>,
    /// KV cache type the server uses, for the fit check.
    #[arg(long, requires = "model", value_parser = cache_name)]
    cache_type: Option<String>,
    /// Ready only when the runtime reports a model whose name contains this.
    /// Without it, any model runtime answering on the port counts.
    #[arg(long)]
    expect: Option<String>,
    /// The server command and its arguments, after `--`. Its output goes to
    /// hearth's stderr; hearth's stdout carries only hearth's own events.
    #[arg(last = true, required = true)]
    command: Vec<OsString>,
}

pub(crate) fn settings(c: &hearth_core::config::Supervise) -> Settings {
    let ms = Duration::from_millis;
    Settings {
        restart: RestartPolicy {
            budget: c.restart_budget,
            window: ms(c.restart_window_ms),
            backoff_start: ms(c.backoff_start_ms),
            backoff_max: ms(c.backoff_max_ms),
            backoff_factor: c.backoff_factor,
        },
        ready_timeout: ms(c.ready_timeout_ms),
        stop_grace: ms(c.stop_grace_ms),
        unresponsive_probes: c.unresponsive_probes,
        output_lines: c.output_lines,
        output_line_bytes: c.output_line_bytes,
    }
}

/// A recorded event as `(who, what happened)`, for `hearth follow`.
pub fn entries(body: &serde_json::Value) -> Vec<(String, String)> {
    let who = format!(
        "{} {} {}",
        body["server"].as_str().unwrap_or(""),
        body["addr"].as_str().unwrap_or("?"),
        body["command"].as_str().unwrap_or("?")
    );
    // The event's own fields tell repeated events apart (code, signal,
    // error, line).
    let mut detail = body["event"].clone();
    if let Some(o) = detail.as_object_mut() {
        o.remove("event");
    }
    vec![(
        who.trim().to_string(),
        format!("{} {detail}", summary(body)),
    )]
}

/// One line for `hearth log`.
pub fn summary(body: &serde_json::Value) -> String {
    let event = &body["event"];
    let decision = event["decision"]
        .as_str()
        .map_or(String::new(), |d| format!(" ({d})"));
    format!(
        "{} {}{decision} · {}",
        body["addr"].as_str().unwrap_or("?"),
        event["event"].as_str().unwrap_or("?"),
        body["command"].as_str().unwrap_or("?")
    )
}

/// How a cause reads in a sentence.
fn cause_name(c: hearth_runtime::signals::Cause) -> &'static str {
    use hearth_runtime::signals::Cause;
    match c {
        Cause::ModelMissing => "model file missing",
        Cause::LoadFailed => "model did not load",
        Cause::OutOfMemory => "out of memory",
        Cause::PortInUse => "port in use",
        Cause::BadArgument => "bad argument",
    }
}

/// Events a person should hear about without watching the terminal.
pub(crate) fn alarming(event: &Event) -> bool {
    let gave_up = |d: &Decision| matches!(d, Decision::GiveUp { .. } | Decision::Futile);
    match event {
        Event::Poisoned { .. } | Event::Wedged { .. } | Event::Lost { .. } => true,
        Event::Exited { decision, .. }
        | Event::NotReady { decision, .. }
        | Event::PortTaken { decision, .. }
        | Event::SpawnFailed { decision, .. } => gave_up(decision),
        Event::Started { .. }
        | Event::Ready { .. }
        | Event::Noticed { .. }
        | Event::Requested { .. }
        | Event::Halted { .. } => false,
    }
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn stopping(error: Option<&String>) -> String {
    error.map_or(String::new(), |e| format!(" (with an error: {e})"))
}

/// Where `program` would be found, the way the shell will look for it: a
/// path is taken as it is, a bare name is searched for in `PATH`. A command
/// that is not there would otherwise start a tether that exits 127, over and
/// over, until the restart budget is spent.
pub(crate) fn find_program(program: &std::ffi::OsStr) -> Result<std::path::PathBuf, String> {
    let path = std::path::Path::new(program);
    let executable = |p: &std::path::Path| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        }
        #[cfg(not(unix))]
        p.is_file()
    };
    if path.components().count() > 1 {
        return if executable(path) {
            Ok(path.to_path_buf())
        } else {
            Err(format!("{} is not an executable file", path.display()))
        };
    }
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(path))
        .find(|candidate| executable(candidate))
        .ok_or_else(|| format!("{} was not found in PATH", path.display()))
}

/// The pid listening on `port` among the processes of `tether`'s process
/// group: the server, under its tether and any wrapper script, and never a
/// process someone else runs on the same port. The lowest such pid, so a
/// pre-forking server is named by its first process. An error when the
/// processes cannot be listed.
pub(crate) fn listener_of(port: u16, tether: u32) -> Result<Option<i32>, String> {
    let Ok(group) = i32::try_from(tether) else {
        return Ok(None);
    };
    let processes = hearth_sense::list_processes().map_err(|e| e.to_string())?;
    Ok(processes
        .into_iter()
        .filter(|p| hearth_sense::process_group(p.pid).is_ok_and(|g| g == group))
        .filter(|p| {
            hearth_sense::tcp_sockets(p.pid).is_ok_and(|sockets| {
                sockets
                    .iter()
                    .any(|s| s.state == hearth_sense::TcpState::Listen && s.local.port() == port)
            })
        })
        .map(|p| p.pid)
        .min())
}

/// Adds to a ready event's record what the server held in memory at the
/// moment it became ready (`footprint_at_ready`), so a later fit of the
/// same model can say what a start of it measured. `listener` is what
/// [`listener_of`] found; when there is no footprint, the reason is
/// recorded instead.
pub(crate) fn footprint_at_ready(
    record: &mut serde_json::Value,
    listener: &Result<Option<i32>, String>,
) {
    let error = |e: String| serde_json::json!({ "error": e });
    record["footprint_at_ready"] = match listener {
        Ok(Some(pid)) => match hearth_sense::process_footprint(*pid) {
            Ok(f) => serde_json::to_value(f).unwrap_or_else(|e| error(e.to_string())),
            Err(e) => error(e.to_string()),
        },
        Ok(None) => error("no process of the server's group listens on its port".into()),
        Err(e) => error(format!("cannot list processes: {e}")),
    };
}

/// The fit check run before each start, when --model, --ctx and
/// --cache-type say what the server loads; the projector its command
/// loads counts too, where it can be found.
fn fit_check(
    args: &Args,
    addr: std::net::SocketAddr,
) -> Option<impl Fn() -> Result<(), String> + Send + use<>> {
    let words: Vec<String> = args
        .command
        .iter()
        .map(|w| w.to_string_lossy().into_owned())
        .collect();
    let projector = crate::fit::projector_arg(&words).filter(|p| p.is_file());
    args.model
        .clone()
        .zip(args.ctx)
        .zip(args.cache_type.clone())
        .map(|((m, c), t)| crate::fit::start_check(format!("[{addr}]"), (m, c, t), projector))
}

/// What `hearth run` records of one event.
fn journal_record(
    addr: std::net::SocketAddr,
    expect: Option<&str>,
    program: &str,
    t_ms: u64,
    event: &Event,
) -> serde_json::Value {
    let mut record = serde_json::json!({
        "addr": addr,
        "expect": expect,
        "command": program,
        "t_ms": t_ms,
        "event": event,
    });
    if let Event::Ready { pid, .. } = event {
        footprint_at_ready(&mut record, &listener_of(addr.port(), *pid));
    }
    record
}

/// What the supervisor does next, in words.
fn next(d: &Decision) -> String {
    match d {
        Decision::Restart { after_ms } => format!("restarting in {after_ms} ms"),
        Decision::GiveUp { exits, window_ms } => {
            format!("{exits} exits within {window_ms} ms: a crash loop, giving up")
        }
        Decision::Futile => {
            "a restart cannot fix that: not restarting; fix the command, then hearth start".into()
        }
    }
}

pub(crate) fn describe(event: &Event) -> String {
    match event {
        Event::Started { pid } => format!("started pid {pid}"),
        Event::Ready {
            pid,
            after_ms,
            identity,
            compute_bytes,
        } => format!(
            "pid {pid} ready after {after_ms} ms, serving {}{}",
            identity.models().join(", "),
            compute_bytes.map_or_else(String::new, |b| format!(
                " (compute buffers {})",
                crate::units::bytes(b)
            ))
        ),
        Event::Exited {
            code,
            signal,
            cause,
            cause_line,
            decision,
        } => format!(
            "exited with {}{}; {}",
            match (code, signal) {
                (_, Some(s)) => format!("signal {s}"),
                (Some(c), None) => format!("code {c}"),
                (None, None) => "no status".into(),
            },
            match (cause, cause_line) {
                (Some(c), Some(line)) => format!(" ({}: {line})", cause_name(*c)),
                _ => String::new(),
            },
            next(decision)
        ),
        Event::NotReady {
            after_ms,
            last,
            stop_error,
            decision,
        } => format!(
            "not ready after {after_ms} ms (last answer: {}); stopped{}; {}",
            json(last),
            stopping(stop_error.as_ref()),
            next(decision)
        ),
        Event::Wedged {
            misses,
            last,
            stop_error,
            decision,
        } => format!(
            "not serving for {misses} probes in a row (last answer: {}): wedged; stopped{}; {}",
            json(last),
            stopping(stop_error.as_ref()),
            next(decision)
        ),
        Event::PortTaken { identity, decision } => format!(
            "port taken by something else ({}); not started; {}",
            json(identity),
            next(decision)
        ),
        Event::SpawnFailed { error, decision } => {
            format!("could not start: {error}; {}", next(decision))
        }
        Event::Poisoned {
            line,
            stop_error,
            decision,
        } => format!(
            "backend poisoned (\"{line}\"): it fails every request until restarted; stopped{}; {}",
            stopping(stop_error.as_ref()),
            next(decision)
        ),
        Event::Noticed { signal, line } => format!("noticed {}: {line}", json(signal)),
        Event::Halted { stop_error } => format!(
            "stopped on request{}; stays down until started",
            stopping(stop_error.as_ref())
        ),
        Event::Requested { stop_error } => format!(
            "restart requested; stopped{}; starting again",
            stopping(stop_error.as_ref())
        ),
        Event::Lost {
            error,
            stop_error,
            decision,
        } => format!(
            "could not check on the server: {error}; stopped{}; {}",
            stopping(stop_error.as_ref()),
            next(decision)
        ),
    }
}

/// Records `record` in the journal, and notifies when `event` is alarming.
/// Failures are reported on stderr: the server is supervised either way.
pub(crate) fn keep(
    journal: Option<&Journal>,
    notify: &hearth_core::config::Notify,
    sending: &mut Vec<std::thread::JoinHandle<()>>,
    (title, event, record): (&str, &Event, &serde_json::Value),
) {
    if let Some(journal) = journal
        && let Err(e) = journal.record(KIND, record)
    {
        eprintln!("hearth: event not recorded: {e}");
    }
    if alarming(event) {
        sending.retain(|h| !h.is_finished());
        sending.extend(crate::notify::send(notify, title, &describe(event), record));
    }
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let probe = Probe {
        addr: SocketAddr::new(args.host, args.port),
        deadline: Duration::from_millis(config.discovery.first_contact_timeout_ms),
        max_bytes: config.discovery.max_probe_response_bytes,
    };
    match preflight(&probe, args.expect.as_deref()) {
        Preflight::Free => {}
        Preflight::AlreadyServing { identity } => {
            return Ok(format!(
                "{} already serves {}; nothing started\n",
                probe.addr,
                identity.models().join(", ")
            ));
        }
        Preflight::Occupied { identity } => {
            return Err(format!(
                "{} is taken: {}; hearth does not touch servers it did not start. Pick another --port, or stop that server yourself",
                probe.addr,
                serde_json::to_string(&identity).unwrap_or_default()
            ));
        }
    }
    let (program, rest) = args.command.split_first().ok_or("no command given")?;
    find_program(program)?;
    // Only the program is kept: arguments can carry keys, and events go to
    // the journal and to webhooks.
    let program_name = program.to_string_lossy().into_owned();
    let (program, rest) = (program.clone(), rest.to_vec());
    let journal = Journal::open(&config.state);
    if let Err(e) = &journal {
        eprintln!("hearth: run events will not be recorded: {e}");
    }
    let mut sending = Vec::new();
    let check = fit_check(args, probe.addr);
    let make = Box::new(move || {
        if let Some(check) = &check {
            check()?;
        }
        let mut c = Command::new(&program);
        c.args(&rest);
        Ok(c)
    });
    let mut supervised = Supervised::new(
        probe,
        args.expect.clone(),
        settings(&config.supervise),
        make,
    );
    let interval = Duration::from_millis(config.supervise.probe_interval_ms);
    let started = Instant::now();
    // Written, not printed: a reader that goes away (`| head`) must end the
    // run with an error, which stops the server, rather than a panic.
    let mut out = std::io::stdout().lock();
    let mut last = None;
    let result = (|| -> Result<(), String> {
        while !supervised.gave_up() {
            let polled = Instant::now();
            if let Some(event) = supervised.poll() {
                last = Some(describe(&event));
                let t_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let record = journal_record(
                    probe.addr,
                    args.expect.as_deref(),
                    &program_name,
                    t_ms,
                    &event,
                );
                keep(
                    journal.as_ref().ok(),
                    &config.notify,
                    &mut sending,
                    (&format!("hearth: {}", probe.addr), &event, &record),
                );
                let line = if json {
                    let mut v = serde_json::to_value(&event).map_err(|e| e.to_string())?;
                    v["t_ms"] = t_ms.into();
                    v.to_string()
                } else {
                    format!("[{}] {}", probe.addr, last.as_deref().unwrap_or_default())
                };
                writeln!(out, "{line}")
                    .and_then(|()| out.flush())
                    .map_err(|e| format!("output closed ({e}); stopping the server"))?;
            }
            std::thread::sleep(interval.saturating_sub(polled.elapsed()));
        }
        Ok(())
    })();
    for s in sending {
        // A delivery thread that panicked has already said so on stderr.
        let _ = s.join();
    }
    result?;
    Err(format!(
        "gave up on the server at {} after {} restarts: {}",
        probe.addr,
        supervised.restarts(),
        last.as_deref().unwrap_or("no event")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_trouble_is_alarming() {
        let restart = Decision::Restart { after_ms: 1 };
        let give_up = Decision::GiveUp {
            exits: 2,
            window_ms: 1,
        };
        let exited = |decision| Event::Exited {
            code: Some(1),
            signal: None,
            cause: None,
            cause_line: None,
            decision,
        };
        assert!(!alarming(&exited(restart)));
        assert!(alarming(&exited(give_up)));
        assert!(alarming(&Event::Poisoned {
            line: String::new(),
            stop_error: None,
            decision: restart,
        }));
        assert!(!alarming(&Event::Started { pid: 1 }));
    }

    #[test]
    fn log_summary_names_the_event_and_decision() {
        let body = serde_json::json!({
            "addr": "127.0.0.1:8081",
            "command": "llama-server -m m.gguf",
            "event": { "event": "exited", "decision": "give_up" },
        });
        assert_eq!(
            summary(&body),
            "127.0.0.1:8081 exited (give_up) · llama-server -m m.gguf"
        );
    }
}
