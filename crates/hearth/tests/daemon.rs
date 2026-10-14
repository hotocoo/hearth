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
    let dir = home("e2e");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let script = dir.join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "[servers.e2e]\nport = {port}\nexpect = \"e2e\"\ncommand = [\"python3\", \"{}\", \"{port}\"]\n",
            script.display()
        ),
    )
    .unwrap();
    assert!(
        hearth(&dir, &["status"])
            .unwrap_err()
            .contains("not running"),
        "no daemon yet"
    );
    let mut daemon = Killed(
        Command::new(env!("CARGO_BIN_EXE_hearthd"))
            .env("HEARTH_STATE", dir.join("state"))
            .env("HEARTH_CONFIG", dir.join("hearth.toml"))
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait_until("ready", || state(&dir).is_some_and(|(s, _)| s == "ready"));
    let (_, first) = state(&dir).unwrap();

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
