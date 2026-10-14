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
print("e2e server starting", flush=True)
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        b = json.dumps({"data": [{"id": "e2e-model"}]}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
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

fn config(dir: &Path, port: u16) {
    let script = dir.join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "[control]\ntimeout_ms = 1000\nmax_message_bytes = 1024\n[servers.e2e]\nport = {port}\nexpect = \"e2e\"\ncommand = [\"python3\", \"{}\", \"{port}\"]\n",
            script.display()
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
    let port = free_port();
    config(&dir, port);
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
    hearth(&dir, &["stop", "e2e"]).unwrap();
    wait_until("stopped", || {
        state(&dir).is_some_and(|(s, _)| s == "stopped") && !listening(port)
    });
    hearth(&dir, &["start", "e2e"]).unwrap();
    wait_until("ready again", || {
        state(&dir).is_some_and(|(s, _)| s == "ready")
    });
    // What the server printed is kept, across its restarts.
    let logs = hearth(&dir, &["logs", "e2e"]).unwrap();
    let starts = logs["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| l.as_str() == Some("e2e server starting"))
        .count();
    assert!(starts >= 2, "{logs}");

    check_reload(&dir);

    // hearthd watches the machine too, so a second watcher is refused.
    assert!(
        hearth(&dir, &["watch", "--for-seconds", "1"])
            .unwrap_err()
            .contains("already watches"),
        "hearth watch beside hearthd"
    );

    // One hearthd per state directory.
    let second = hearthd(&dir).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already runs"));

    // A client that sends too much, or stalls, gets an error, and hearthd
    // keeps answering everyone else.
    let sock = dir.join("state").join("hearthd.sock");
    let mut big = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    let _ = std::io::Write::write_all(&mut big, &[b'x'; 4096]);
    let mut reply = String::new();
    let _ = std::io::Read::read_to_string(&mut big, &mut reply);
    assert!(reply.contains("longer than"), "{reply}");
    let stalled = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    assert!(
        hearth(&dir, &["status"]).is_ok(),
        "served after a stalled client"
    );
    drop(stalled);

    // SIGKILL: no handler runs, and the server must go anyway.
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    wait_until("the server to stop", || !listening(port));
    // What it did is in the journal.
    let log = hearth(&dir, &["log", "--kind", "run"]).unwrap();
    let events: Vec<&str> = log["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["summary"].as_str())
        .collect();
    assert!(events.iter().any(|s| s.contains("requested")), "{events:?}");
}

#[test]
fn hearthd_never_removes_what_is_not_its_socket() {
    let dir = home("notsock");
    config(&dir, free_port());
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
    config(&dir, free_port());
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
