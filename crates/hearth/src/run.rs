//! `hearth run`: start a model server and keep it alive.
//!
//! hearth owns what it starts here, and only that: the port is asked who is
//! there first, and a port that another process already serves is never
//! touched. The server runs under a tether, so it stops when hearth does,
//! however hearth stops.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::time::{Duration, Instant};

use hearth_runtime::supervise::{
    Decision, Event, Preflight, Probe, RestartPolicy, Settings, Supervised, preflight,
};

#[derive(clap::Args)]
pub struct Args {
    /// Port the server listens on, which hearth probes.
    #[arg(long)]
    port: u16,
    /// Address to probe the port on.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    host: IpAddr,
    /// Ready only when the runtime reports a model whose name contains this.
    /// Without it, any model runtime answering on the port counts.
    #[arg(long)]
    expect: Option<String>,
    /// The server command and its arguments, after `--`.
    #[arg(last = true, required = true)]
    command: Vec<OsString>,
}

fn settings(c: &hearth_core::config::Supervise) -> Settings {
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
fn find_program(program: &std::ffi::OsStr) -> Result<std::path::PathBuf, String> {
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

fn describe(event: &Event) -> String {
    let next = |d: &Decision| match d {
        Decision::Restart { after_ms } => format!("restarting in {after_ms} ms"),
        Decision::GiveUp { exits, window_ms } => {
            format!("{exits} exits within {window_ms} ms: a crash loop, giving up")
        }
    };
    match event {
        Event::Started { pid } => format!("started pid {pid}"),
        Event::Ready {
            pid,
            after_ms,
            identity,
        } => format!(
            "pid {pid} ready after {after_ms} ms, serving {}",
            identity.models().join(", ")
        ),
        Event::Exited {
            code,
            signal,
            decision,
        } => format!(
            "exited with {}; {}",
            match (code, signal) {
                (_, Some(s)) => format!("signal {s}"),
                (Some(c), None) => format!("code {c}"),
                (None, None) => "no status".into(),
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
    let (program, rest) = (program.clone(), rest.to_vec());
    let make = Box::new(move || {
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
    while !supervised.gave_up() {
        let polled = Instant::now();
        if let Some(event) = supervised.poll() {
            let t_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let line = if json {
                let mut v = serde_json::to_value(&event).map_err(|e| e.to_string())?;
                v["t_ms"] = t_ms.into();
                v.to_string()
            } else {
                let mut s = String::new();
                let _ = write!(s, "[{}] {}", probe.addr, describe(&event));
                s
            };
            writeln!(out, "{line}")
                .and_then(|()| out.flush())
                .map_err(|e| format!("output closed ({e}); stopping the server"))?;
        }
        std::thread::sleep(interval.saturating_sub(polled.elapsed()));
    }
    Err(format!(
        "gave up on the server at {} after {} restarts",
        probe.addr,
        supervised.restarts()
    ))
}
