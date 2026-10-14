//! The supervisor against real processes: a few lines of stdlib Python that
//! answer like an OpenAI-compatible server, so no model is needed. A fake
//! can exit or stop answering when the test says so (through a trigger file,
//! so slow machines cannot race a timer), ignore SIGTERM the way a latched
//! llama.cpp backend does, and run behind a wrapper script.

#![allow(clippy::unwrap_used)]

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hearth_runtime::fingerprint::Identity;
use hearth_runtime::supervise::{
    Decision, Event, Preflight, Probe, RestartPolicy, Settings, Supervised, preflight, tethered,
};

/// argv: port, model id, trigger file, mode (`exit`, `wedge` or `poison`),
/// ignore SIGTERM. Each request checks the trigger: in `exit` mode the server
/// exits with code 3 and removes it, in `wedge` mode it stops answering (the
/// file stays), in `poison` mode it keeps answering and logs what llama.cpp's
/// Metal backend logs on every request once it has latched.
const FAKE: &str = r#"
import sys, json, os, signal, time
from http.server import BaseHTTPRequestHandler, HTTPServer
port, ident, trigger, mode, ignore_term = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5] == "1"
if ignore_term:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if os.path.exists(trigger):
            if mode == "exit":
                os.remove(trigger)
                os._exit(3)
            if mode == "poison":
                print("ggml_metal_graph_compute: backend is in error state from a previous command buffer failure - recreate the backend to recover", file=sys.stderr, flush=True)
            else:
                time.sleep(3600)
        b = json.dumps({"data": [{"id": ident}]}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", port), H).serve_forever()
"#;

/// Generous for slow CI machines; the tests stop as soon as they have seen
/// what they wait for.
const LIMIT: Duration = Duration::from_secs(60);

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn trigger(test: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("hearth-fake-{test}-{}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

struct Fake {
    id: &'static str,
    trigger: PathBuf,
    mode: &'static str,
    ignore_term: bool,
}

impl Fake {
    fn new(id: &'static str, trigger: &Path, mode: &'static str) -> Self {
        Self {
            id,
            trigger: trigger.to_path_buf(),
            mode,
            ignore_term: false,
        }
    }

    fn args(&self, port: u16) -> Vec<String> {
        vec![
            "-c".into(),
            FAKE.into(),
            port.to_string(),
            self.id.into(),
            self.trigger.display().to_string(),
            self.mode.into(),
            if self.ignore_term { "1" } else { "0" }.into(),
        ]
    }

    fn command(&self, port: u16) -> Command {
        let mut c = Command::new("python3");
        c.args(self.args(port));
        c
    }

    fn make(self, port: u16) -> Box<dyn Fn() -> Result<Command, String>> {
        Box::new(move || Ok(self.command(port)))
    }
}

fn probe(port: u16) -> Probe {
    Probe {
        addr: SocketAddr::from(([127, 0, 0, 1], port)),
        deadline: Duration::from_millis(500),
        max_bytes: 1 << 16,
    }
}

fn settings() -> Settings {
    Settings {
        restart: RestartPolicy {
            budget: 2,
            window: Duration::from_secs(60),
            backoff_start: Duration::from_millis(50),
            backoff_max: Duration::from_millis(200),
            backoff_factor: 2,
        },
        ready_timeout: Duration::from_secs(30),
        stop_grace: Duration::from_millis(500),
        unresponsive_probes: 2,
    }
}

/// Polls until `done`, calling `react` on every event so a test can act.
fn drive(
    s: &mut Supervised,
    mut react: impl FnMut(&Event),
    done: impl Fn(&[Event]) -> bool,
) -> Vec<Event> {
    let t = Instant::now();
    let mut seen = Vec::new();
    while t.elapsed() < LIMIT && !done(&seen) {
        if let Some(e) = s.poll() {
            react(&e);
            seen.push(e);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    seen
}

fn wait_for(port: u16, want: impl Fn(&Preflight) -> bool) -> Preflight {
    let t = Instant::now();
    loop {
        let p = preflight(&probe(port), Some("x"));
        if want(&p) || t.elapsed() > LIMIT {
            return p;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn free(p: &Preflight) -> bool {
    matches!(p, Preflight::Free)
}

#[test]
fn a_server_that_exits_is_restarted_and_a_crash_loop_is_given_up() {
    let port = free_port();
    let t = trigger("crash");
    let mut s = Supervised::new(
        probe(port),
        Some("fake-model".into()),
        settings(),
        Fake::new("fake-model", &t, "exit").make(port),
    );
    // Every time it is ready, tell it to exit on its next request.
    let seen = drive(
        &mut s,
        |e| {
            if matches!(e, Event::Ready { .. }) {
                std::fs::write(&t, "").unwrap();
            }
        },
        |seen| {
            seen.iter().any(|e| {
                matches!(
                    e,
                    Event::Exited {
                        decision: Decision::GiveUp { .. },
                        ..
                    }
                )
            })
        },
    );
    let readies = seen
        .iter()
        .filter(|e| matches!(e, Event::Ready { .. }))
        .count();
    let exits: Vec<(Option<i32>, Decision)> = seen
        .iter()
        .filter_map(|e| match e {
            Event::Exited { code, decision, .. } => Some((*code, *decision)),
            _ => None,
        })
        .collect();
    assert!(s.gave_up(), "{seen:?}");
    assert_eq!(readies, 3, "{seen:?}");
    assert_eq!(
        exits,
        [
            (Some(3), Decision::Restart { after_ms: 50 }),
            (Some(3), Decision::Restart { after_ms: 100 }),
            (
                Some(3),
                Decision::GiveUp {
                    exits: 3,
                    window_ms: 60_000
                }
            )
        ],
        "{seen:?}"
    );
    assert_eq!(s.restarts(), 2);
}

#[test]
fn a_port_answered_by_another_model_is_refused_by_name() {
    let port = free_port();
    let t = trigger("refused");
    assert!(free(&preflight(&probe(port), Some("wanted"))));
    let _other = Reaped(
        Fake::new("someone-else", &t, "exit")
            .command(port)
            .spawn()
            .unwrap(),
    );
    wait_for(port, |p| !free(p));
    match preflight(&probe(port), Some("wanted")) {
        Preflight::Occupied { identity } => assert_eq!(identity.models(), ["someone-else"]),
        p => panic!("{p:?}"),
    }
    assert!(matches!(
        preflight(&probe(port), Some("someone-else")),
        Preflight::AlreadyServing { .. }
    ));
    // A supervisor finding the port taken starts nothing and counts it.
    let mut s = Supervised::new(
        probe(port),
        Some("wanted".into()),
        settings(),
        Fake::new("wanted", &t, "exit").make(port),
    );
    let seen = drive(&mut s, |_| {}, |seen| !seen.is_empty());
    assert!(
        matches!(
            seen[0],
            Event::PortTaken {
                decision: Decision::Restart { .. },
                ..
            }
        ),
        "{seen:?}"
    );
}

/// Kills a process the test started directly, even when an assert fails.
struct Reaped(std::process::Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_server_that_never_serves_the_expected_model_counts_as_a_failed_start() {
    let port = free_port();
    let t = trigger("impostor");
    let settings = Settings {
        ready_timeout: Duration::from_secs(5),
        ..settings()
    };
    let mut s = Supervised::new(
        probe(port),
        Some("wanted".into()),
        settings,
        Fake::new("impostor", &t, "exit").make(port),
    );
    // A slow interpreter start can time out before the impostor answers; the
    // test waits for a timeout on the impostor itself.
    let on_impostor = |e: &Event| {
        matches!(
            e,
            Event::NotReady {
                last: Identity::OpenAiCompatible { .. },
                ..
            }
        )
    };
    let seen = drive(&mut s, |_| {}, |seen| seen.iter().any(on_impostor));
    assert!(
        seen.iter().any(|e| matches!(
            e,
            Event::NotReady {
                last: Identity::OpenAiCompatible { .. },
                decision: Decision::Restart { .. },
                stop_error: None,
                ..
            }
        )),
        "{seen:?}"
    );
    assert!(!s.ready());
}

/// A wedged server that also ignores SIGTERM: found by its silence, stopped
/// by the SIGKILL escalation, and started again.
#[cfg(unix)]
#[test]
fn a_silent_server_is_killed_even_if_it_ignores_sigterm_and_restarted() {
    let port = free_port();
    let t = trigger("wedge");
    let fake = Fake {
        ignore_term: true,
        ..Fake::new("fake-model", &t, "wedge")
    };
    let mut s = Supervised::new(
        probe(port),
        Some("fake-model".into()),
        settings(),
        fake.make(port),
    );
    let mut readies = 0;
    let seen = drive(
        &mut s,
        |e| match e {
            Event::Ready { .. } => {
                readies += 1;
                if readies == 1 {
                    std::fs::write(&t, "").unwrap();
                }
            }
            Event::Wedged { .. } => std::fs::remove_file(&t).unwrap(),
            _ => {}
        },
        |seen| {
            seen.iter()
                .filter(|e| matches!(e, Event::Ready { .. }))
                .count()
                >= 2
        },
    );
    let wedged = seen
        .iter()
        .position(|e| matches!(e, Event::Wedged { .. }))
        .unwrap_or_else(|| panic!("{seen:?}"));
    assert!(
        matches!(
            seen[wedged],
            Event::Wedged {
                misses: 2,
                last: Identity::Unresponsive { .. },
                stop_error: None,
                decision: Decision::Restart { .. }
            }
        ),
        "{seen:?}"
    );
    assert!(
        seen[wedged..]
            .iter()
            .any(|e| matches!(e, Event::Ready { .. })),
        "restarted after the wedge: {seen:?}"
    );
    s.stop().unwrap();
    assert!(free(&wait_for(port, free)));
}

/// A supervisor that dies, SIGKILL included, takes its server with it: the
/// kernel closes the supervisor's end of the tether's pipe, which is what
/// dropping it here does. The server runs behind a wrapper script that does
/// not `exec` it and ignores SIGTERM, so only the group SIGKILL reaches it.
#[cfg(unix)]
#[test]
fn a_wrapped_server_stops_when_its_supervisors_pipe_closes() {
    let port = free_port();
    let t = trigger("tether");
    let fake = Fake {
        ignore_term: true,
        ..Fake::new("tethered", &t, "exit")
    };
    let mut wrapper = Command::new("sh");
    wrapper
        .arg("-c")
        .arg("python3 \"$@\"; echo wrapper-done")
        .arg("wrapper")
        .args(fake.args(port));
    let mut tether = tethered(&wrapper, Duration::from_millis(300))
        .spawn()
        .unwrap();
    assert!(!free(&wait_for(port, |p| !free(p))), "never started");
    drop(tether.stdin.take());
    assert!(
        free(&wait_for(port, free)),
        "the server outlived its supervisor"
    );
    let status = tether.wait().unwrap();
    assert!(
        !status.success(),
        "the tether reports the server's end: {status:?}"
    );
}

/// A latched backend keeps answering; only its log says it is dead.
#[test]
fn a_server_whose_log_says_it_is_poisoned_is_restarted() {
    let port = free_port();
    let t = trigger("poison");
    let mut s = Supervised::new(
        probe(port),
        Some("fake-model".into()),
        settings(),
        Fake::new("fake-model", &t, "poison").make(port),
    );
    let mut readies = 0;
    let seen = drive(
        &mut s,
        |e| match e {
            Event::Ready { .. } => {
                readies += 1;
                if readies == 1 {
                    std::fs::write(&t, "").unwrap();
                }
            }
            Event::Poisoned { .. } => std::fs::remove_file(&t).unwrap(),
            _ => {}
        },
        |seen| {
            seen.iter()
                .filter(|e| matches!(e, Event::Ready { .. }))
                .count()
                >= 2
        },
    );
    let poisoned = seen
        .iter()
        .position(|e| matches!(e, Event::Poisoned { .. }))
        .unwrap_or_else(|| panic!("{seen:?}"));
    let Event::Poisoned {
        line,
        stop_error: None,
        decision: Decision::Restart { .. },
    } = &seen[poisoned]
    else {
        panic!("{seen:?}")
    };
    assert!(line.contains("recreate the backend"), "{line}");
    assert!(
        seen[poisoned..]
            .iter()
            .any(|e| matches!(e, Event::Ready { .. })),
        "restarted after the poison: {seen:?}"
    );
}
