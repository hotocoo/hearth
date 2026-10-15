//! hearthd and the hearth CLI end to end, as built: a real server (a few
//! lines of stdlib Python) is started, reported, restarted on request, and
//! gone once hearthd is killed with SIGKILL.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

const SERVER: &str = r#"
import sys, json
import os
print("e2e server starting", flush=True)
print("mark", os.environ.get("E2E_MARK"), "in", os.getcwd(), flush=True)
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/slow":
            self.send_response(200); self.end_headers()
            try:
                for _ in range(600):
                    self.wfile.write(b"x\n"); self.wfile.flush(); time.sleep(0.05)
            except OSError:
                open("cancelled", "w").close()
            return
        b = json.dumps({"data": [{"id": "e2e-model"}]}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
"#;

const LIMIT: Duration = Duration::from_secs(60);

/// A short directory: socket paths are limited to about a hundred bytes.
fn home(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("d-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `python3` runs the fake servers; without it the test cannot run here.
fn python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn hearthd(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hearthd"));
    c.env("HEARTH_STATE", dir.join("state"))
        .env("HEARTH_CONFIG", dir.join("hearth.toml"));
    c
}

/// The control socket's limits, small so the test can pass them; the
/// oversized request and the stalled client are sized from these.
const MAX_MESSAGE_BYTES: usize = 1024;
const CONTROL_TIMEOUT_MS: u64 = 1000;
/// Watch rounds short enough that hearthd records several during the test.
const DOCTOR_INTERVAL_MS: u64 = 200;
const PORTS_INTERVAL_MS: u64 = 1000;

/// `metrics` is where hearthd serves `/metrics`, `gateway` its gateway.
fn config(dir: &Path, port: u16, metrics: u16, gateway: u16) {
    let script = dir.join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "[control]\ntimeout_ms = {CONTROL_TIMEOUT_MS}\nmax_message_bytes = {MAX_MESSAGE_BYTES}\n[watch]\ndoctor_interval_ms = {DOCTOR_INTERVAL_MS}\nports_interval_ms = {PORTS_INTERVAL_MS}\n[metrics]\nlisten = \"127.0.0.1:{metrics}\"\n[gateway]\nlisten = \"127.0.0.1:{gateway}\"\n[servers.e2e]\nport = {port}\nexpect = \"e2e\"\ncommand = [\"python3\", \"{}\", \"{port}\"]\ndir = \"{}\"\nenv = {{ E2E_MARK = \"on\" }}\n",
            script.display(),
            dir.display()
        ),
    )
    .unwrap();
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn hearth(dir: &Path, args: &[&str]) -> Result<Value, String> {
    let out = Command::new(env!("CARGO_BIN_EXE_hearth"))
        .arg("--json")
        .args(args)
        .env("HEARTH_STATE", dir.join("state"))
        .env("HEARTH_CONFIG", dir.join("hearth.toml"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))?;
    if out.status.success() {
        Ok(v)
    } else {
        Err(v["error"].as_str().unwrap_or_default().to_string())
    }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < LIMIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn state(dir: &Path) -> Option<(String, Option<u64>)> {
    let v = hearth(dir, &["status"]).ok()?;
    let s = &v["servers"][0];
    Some((s["state"].as_str()?.to_string(), s["pid"].as_u64()))
}

fn listening(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

/// Whether hearthd's `/metrics` on `port` has `server` in `state`.
fn gauge(port: u16, server: &str, state: &str) -> bool {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nHost: test\r\n\r\n")
        .unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    reply.contains(&format!(
        "hearth_server_state{{server=\"{server}\",state=\"{state}\"}} 1"
    ))
}

/// One request through hearthd's gateway, as raw HTTP.
fn http(port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(s, "GET {path} HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    reply
}

/// Stopped on request, the server frees its port; metrics say so, and the
/// gateway tells a client how to start it.
fn check_stopped(dir: &Path, port: u16, metrics: u16, gateway: u16) {
    hearth(dir, &["stop", "e2e"]).unwrap();
    wait_until("stopped", || {
        state(dir).is_some_and(|(s, _)| s == "stopped") && !listening(port)
    });
    assert!(gauge(metrics, "e2e", "stopped"), "metrics while stopped");
    gateway_says(
        gateway,
        "/s/e2e/v1/models",
        "HTTP/1.1 503",
        "hearth start e2e",
    );
}

/// The gateway answers `path` with `status` and a reply mentioning `says`.
fn gateway_says(gateway: u16, path: &str, status: &str, says: &str) {
    let reply = http(gateway, path);
    assert!(reply.starts_with(status) && reply.contains(says), "{reply}");
}

/// The gateway forwards to a ready server by name, says what to do about a
/// name it does not keep, and closes the server's connection when the
/// client goes.
fn check_gateway(dir: &Path, gateway: u16) {
    use std::io::{Read, Write};
    let ok = http(gateway, "/s/e2e/v1/models");
    assert!(
        ok.starts_with("HTTP/1.0 200") && ok.contains("e2e-model"),
        "{ok}"
    );
    gateway_says(gateway, "/s/nope/v1/models", "HTTP/1.1 404", "servers: e2e");
    let mut slow = std::net::TcpStream::connect(("127.0.0.1", gateway)).unwrap();
    write!(slow, "GET /s/e2e/slow HTTP/1.1\r\nHost: test\r\n\r\n").unwrap();
    let mut first = [0u8; 64];
    assert!(slow.read(&mut first).unwrap() > 0, "the stream began");
    drop(slow);
    wait_until("the server to see its client go", || {
        dir.join("cancelled").exists()
    });
}

/// hearthd watches the machine too: its rounds are in the journal, and a
/// second watcher is refused.
fn check_watching(dir: &Path) {
    for kind in ["doctor", "ports"] {
        wait_until(&format!("a {kind} round"), || {
            hearth(dir, &["log", "--kind", kind])
                .is_ok_and(|v| v["snapshots"].as_array().is_some_and(|a| !a.is_empty()))
        });
    }
    assert!(
        hearth(dir, &["watch", "--for-seconds", "1"])
            .unwrap_err()
            .contains("already watches"),
        "hearth watch beside hearthd"
    );
}

/// An agent reaches the same servers through `hearth mcp`: status lists
/// them, and restart restarts one.
fn check_mcp(dir: &Path) {
    use std::io::Write;
    let (_, before) = state(dir).unwrap();
    let mut mcp = Command::new(env!("CARGO_BIN_EXE_hearth"))
        .arg("mcp")
        .env("HEARTH_STATE", dir.join("state"))
        .env("HEARTH_CONFIG", dir.join("hearth.toml"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "status", "arguments": {}}}),
        serde_json::json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "restart", "arguments": {"server": "e2e"}}}),
    ];
    let mut stdin = mcp.stdin.take().unwrap();
    for r in &requests {
        writeln!(stdin, "{r}").unwrap();
    }
    drop(stdin);
    let out = mcp.wait_with_output().unwrap();
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let result = |id: u64| {
        let r = replies.iter().find(|r| r["id"] == id).unwrap();
        assert_eq!(r["result"]["isError"], false, "{r}");
        r["result"]["structuredContent"].clone()
    };
    assert_eq!(result(2)["servers"][0]["name"], "e2e");
    result(3);
    wait_until("the MCP restart", || {
        state(dir).is_some_and(|(s, pid)| s == "ready" && pid != before)
    });
}

/// `hearth top` shows hearthd's server, in one frame (`--once`, `--json`)
/// and on a terminal, where `r` restarts the selected server and `q` quits.
fn check_top(dir: &Path) {
    use std::io::{Read, Write};
    use std::process::Stdio;
    use std::sync::{Arc, Mutex};

    let name = hearth(dir, &["status"]).unwrap()["servers"][0]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let frame = hearth(dir, &["top"]).unwrap();
    assert_eq!(frame["servers"][0]["name"], name.as_str(), "{frame}");
    assert!(frame["machine"]["health"].is_object(), "{frame}");
    let once = Command::new(env!("CARGO_BIN_EXE_hearth"))
        .args(["top", "--once"])
        .env("HEARTH_STATE", dir.join("state"))
        .env("HEARTH_CONFIG", dir.join("hearth.toml"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&once.stdout);
    assert!(once.status.success(), "{text}");
    assert!(
        text.lines().any(|l| l.trim_start().starts_with(&name)),
        "{text}"
    );

    // script(1) gives top a terminal; its arguments differ by system.
    let bin = env!("CARGO_BIN_EXE_hearth");
    let mut script = Command::new("script");
    if cfg!(target_os = "linux") {
        script.args(["-qec", &format!("'{bin}' top --every 1"), "/dev/null"]);
    } else {
        script.args(["-q", "/dev/null", bin, "top", "--every", "1"]);
    }
    let mut top = Killed(
        script
            .env("HEARTH_STATE", dir.join("state"))
            .env("HEARTH_CONFIG", dir.join("hearth.toml"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let seen = Arc::new(Mutex::new(String::new()));
    let mut stdout = top.0.stdout.take().unwrap();
    let sink = Arc::clone(&seen);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });
    let shows = |what: &str| seen.lock().unwrap().contains(what);
    wait_until("top's first frame", || shows(&format!("> {name}")));
    let before = state(dir).and_then(|(_, pid)| pid);
    let mut keys = top.0.stdin.take().unwrap();
    keys.write_all(b"r").unwrap();
    wait_until("top's restart", || {
        shows(&format!("restart of {name} requested"))
            && state(dir).is_some_and(|(s, pid)| s == "ready" && pid != before)
    });
    keys.write_all(b"q").unwrap();
    drop(keys);
    wait_until("top to quit", || top.0.try_wait().unwrap().is_some());
    assert!(top.0.wait().unwrap().success(), "{}", seen.lock().unwrap());
    // The terminal is put back: the alternate screen is left.
    wait_until("top's last output", || shows("\x1b[?1049l"));
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn hearthd_keeps_reports_restarts_and_takes_its_server_down_with_it() {
    if !python() {
        eprintln!("skipped: python3 is needed for the fake server");
        return;
    }
    let dir = home("e2e");
    let (port, metrics, gateway) = (free_port(), free_port(), free_port());
    config(&dir, port, metrics, gateway);
    assert!(
        hearth(&dir, &["status"])
            .unwrap_err()
            .contains("not running"),
        "no daemon yet"
    );
    let mut daemon = Killed(
        hearthd(&dir)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("ready", || state(&dir).is_some_and(|(s, _)| s == "ready"));
    let (_, first) = state(&dir).unwrap();
    // The listening process is found and measured, not just the tether.
    let status = hearth(&dir, &["status"]).unwrap();
    let s = &status["servers"][0];
    assert!(s["server_pid"].as_u64().is_some(), "{status}");
    assert_ne!(s["server_pid"], s["pid"], "the server is not its tether");
    assert!(
        s["footprint_bytes"].as_u64().is_some_and(|b| b > 0),
        "{status}"
    );

    hearth(&dir, &["restart", "e2e"]).unwrap();
    wait_until("a new pid", || {
        state(&dir).is_some_and(|(s, pid)| s == "ready" && pid != first)
    });
    assert!(
        hearth(&dir, &["restart", "nobody"])
            .unwrap_err()
            .contains("no server named"),
        "unknown names are refused"
    );

    // Stopped on request it stays down, and frees its port; started, it
    // comes back.
    // Metrics say the same as status, at every step.
    check_gateway(&dir, gateway);
    assert!(gauge(metrics, "e2e", "ready"), "metrics while ready");
    check_stopped(&dir, port, metrics, gateway);
    hearth(&dir, &["start", "e2e"]).unwrap();
    wait_until("ready again", || {
        state(&dir).is_some_and(|(s, _)| s == "ready")
    });
    assert!(gauge(metrics, "e2e", "ready"), "metrics once started");
    // What the server printed is kept, across its restarts.
    let logs = hearth(&dir, &["logs", "e2e"]).unwrap();
    let starts = logs["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l.as_str() == Some("e2e server starting"))
        .count();
    assert!(starts >= 2, "{logs}");
    // It ran with its own variables, in its own directory.
    let marked = format!("mark on in {}", dir.display());
    assert!(
        logs["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l.as_str() == Some(marked.as_str())),
        "{logs}"
    );

    check_reload(&dir);
    check_mcp(&dir);
    check_top(&dir);
    check_watching(&dir);

    // One hearthd per state directory.
    let second = hearthd(&dir).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already runs"));

    // A client that sends too much, or stalls, gets an error, and hearthd
    // keeps answering everyone else.
    let sock = dir.join("state").join("hearthd.sock");
    let mut big = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    let _ = std::io::Write::write_all(&mut big, &vec![b'x'; MAX_MESSAGE_BYTES * 4]);
    let mut reply = String::new();
    let _ = std::io::Read::read_to_string(&mut big, &mut reply);
    assert!(reply.contains("longer than"), "{reply}");
    let stalled = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    let asked = Instant::now();
    assert!(
        hearth(&dir, &["status"]).is_ok(),
        "served after a stalled client"
    );
    assert!(
        asked.elapsed() < Duration::from_millis(CONTROL_TIMEOUT_MS),
        "the stalled client held up the next one"
    );
    drop(stalled);

    // SIGKILL: no handler runs, and the server must go anyway.
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    wait_until("the server to stop", || !listening(port));
    journal_has_the_run(&dir);
}

/// What hearthd did is in the journal, and `--grep` finds it with its `~N`.
fn journal_has_the_run(dir: &Path) {
    let log = hearth(dir, &["log", "--kind", "run"]).unwrap();
    let events: Vec<&str> = log["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["summary"].as_str())
        .collect();
    assert!(events.iter().any(|s| s.contains("requested")), "{events:?}");
    // --grep keeps each match's ~N, and --limit counts matches.
    let found = hearth(
        dir,
        &[
            "log",
            "--kind",
            "run",
            "--grep",
            "REQUESTED",
            "--limit",
            "1",
        ],
    )
    .unwrap();
    let found = found["snapshots"].as_array().unwrap();
    assert_eq!(found.len(), 1);
    let back = usize::try_from(found[0]["back"].as_u64().unwrap()).unwrap();
    assert!(found[0]["summary"].as_str().unwrap().contains("requested"));
    assert_eq!(log["snapshots"][back]["id"], found[0]["id"]);
    // A ready event records what the server held once serving.
    let ready = hearth(
        dir,
        &["log", "--kind", "run", "--grep", "ready", "--limit", "1"],
    )
    .unwrap();
    let id = ready["snapshots"][0]["id"].as_str().unwrap();
    let held = hearth(
        dir,
        &[
            "show",
            "--kind",
            "run",
            "--pointer",
            "/body/footprint_at_ready/footprint_bytes",
            id,
        ],
    )
    .unwrap();
    assert!(held.as_u64().is_some_and(|b| b > 0), "{held}");
}

/// With no servers hearthd still watches: it starts, answers on its
/// socket and records rounds. With watching off too it has nothing to do
/// and says so.
#[test]
fn hearthd_without_servers_only_watches() {
    let dir = home("watch-only");
    std::fs::write(
        dir.join("hearth.toml"),
        format!("[watch]\ndoctor_interval_ms = {DOCTOR_INTERVAL_MS}\nports_interval_ms = {PORTS_INTERVAL_MS}\n"),
    )
    .unwrap();
    let _daemon = Killed(
        hearthd(&dir)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("status with no servers", || {
        hearth(&dir, &["status"]).is_ok_and(|v| v["servers"].as_array().is_some_and(Vec::is_empty))
    });
    check_watching(&dir);

    let idle = home("idle");
    std::fs::write(idle.join("hearth.toml"), "[watch]\nin_hearthd = false\n").unwrap();
    let out = hearthd(&idle).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("nothing to do"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn hearthd_never_removes_what_is_not_its_socket() {
    let dir = home("notsock");
    config(&dir, free_port(), free_port(), free_port());
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::set_permissions(
        dir.join("state"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(dir.join("state").join("hearthd.sock"), "precious").unwrap();
    let out = hearthd(&dir).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a socket"));
    assert_eq!(
        std::fs::read_to_string(dir.join("state").join("hearthd.sock")).unwrap(),
        "precious"
    );
}

#[test]
fn hearthd_refuses_a_state_directory_others_can_enter() {
    let dir = home("open");
    config(&dir, free_port(), free_port(), free_port());
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::set_permissions(
        dir.join("state"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let out = hearthd(&dir).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("open to other users"));
}

/// A server added to the configuration starts on reload, and one taken out
/// stops; the server already running keeps its process throughout.
fn check_reload(dir: &Path) {
    let second = free_port();
    let toml = std::fs::read_to_string(dir.join("hearth.toml")).unwrap();
    let script = dir.join("server.py");
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "{toml}[servers.extra]\nport = {second}\ncommand = [\"python3\", \"{}\", \"{second}\"]\n",
            script.display()
        ),
    )
    .unwrap();
    let (_, before) = state(dir).unwrap();
    let plan = hearth(dir, &["reload"]).unwrap();
    assert_eq!(plan["added"], serde_json::json!(["extra"]), "{plan}");
    wait_until("the added server", || listening(second));
    // The reload is done when status lists it; only then is a second one
    // accepted.
    wait_until("status to list it", || {
        hearth(dir, &["status"])
            .is_ok_and(|v| v["servers"].as_array().is_some_and(|a| a.len() == 2))
    });
    std::fs::write(dir.join("hearth.toml"), toml).unwrap();
    assert_eq!(
        hearth(dir, &["reload"]).unwrap()["removed"],
        serde_json::json!(["extra"])
    );
    wait_until("the removed server to stop", || !listening(second));
    assert_eq!(
        state(dir).unwrap().1,
        before,
        "untouched server kept its process"
    );
}

/// Records each POST body it receives as one line of `argv[2]`.
const HOOK: &str = r#"
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        with open(sys.argv[2], "ab") as f: f.write(body + b"\n")
        self.send_response(200); self.send_header("Content-Length", "0"); self.end_headers()
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
"#;

/// Exits a crash loop may hold before hearthd gives up, and back-off short
/// enough that it gets there quickly.
const RESTART_BUDGET: u32 = 1;
const BACKOFF_MS: u64 = 50;

#[test]
fn hearthd_gives_up_on_a_crash_loop_and_says_so_on_the_webhook() {
    let curl = Command::new("curl")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !python() || !curl {
        eprintln!("skipped: python3 and curl needed for the server and the webhook");
        return;
    }
    let dir = home("hook");
    let (port, hook) = (free_port(), free_port());
    let received = dir.join("received.jsonl");
    std::fs::write(dir.join("hook.py"), HOOK).unwrap();
    let _hook = Killed(
        Command::new("python3")
            .arg(dir.join("hook.py"))
            .arg(hook.to_string())
            .arg(&received)
            .spawn()
            .unwrap(),
    );
    wait_until("the webhook", || listening(hook));
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "[supervise]\nrestart_budget = {RESTART_BUDGET}\nbackoff_start_ms = {BACKOFF_MS}\nbackoff_max_ms = {BACKOFF_MS}\n[watch]\nin_hearthd = false\n[notify]\nwebhook_url = \"http://127.0.0.1:{hook}/\"\n[servers.crash]\nport = {port}\ncommand = [\"python3\", \"-c\", \"raise SystemExit(1)\"]\n"
        ),
    )
    .unwrap();
    let _daemon = Killed(
        hearthd(&dir)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("gave up", || {
        state(&dir).is_some_and(|(s, _)| s == "gave_up")
    });
    wait_until("the webhook to hear of it", || {
        std::fs::read_to_string(&received).is_ok_and(|t| {
            t.lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .any(|v| v["server"] == "crash" && v["event"]["decision"] == "give_up")
        })
    });
}
