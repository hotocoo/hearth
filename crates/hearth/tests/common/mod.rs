//! What the end-to-end tests share: a scratch home, the built binaries,
//! a fake model server, and waiting with a limit.

#![allow(dead_code, clippy::unwrap_used)]

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const SERVER: &str = r#"
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
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if self.path == "/flaky" and not os.path.exists("flaked"):
            open("flaked", "w").close()
            os._exit(1)
        b = json.dumps({"got": body.decode()}).encode()
        self.send_response(200); self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self, *a): pass
time.sleep(float(os.environ.get("E2E_DELAY", "0")))
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
"#;

pub const LIMIT: Duration = Duration::from_secs(60);

/// A short directory: socket paths are limited to about a hundred bytes.
pub fn home(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("d-{test}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `python3` runs the fake servers; without it the test cannot run here.
pub fn python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn hearthd(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hearthd"));
    c.env("HEARTH_STATE", dir.join("state"))
        .env("HEARTH_CONFIG", dir.join("hearth.toml"));
    c
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn hearth(dir: &Path, args: &[&str]) -> Result<Value, String> {
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

pub fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let t = Instant::now();
    while !ok() {
        assert!(t.elapsed() < LIMIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn state(dir: &Path) -> Option<(String, Option<u64>)> {
    let v = hearth(dir, &["status"]).ok()?;
    let s = &v["servers"][0];
    Some((s["state"].as_str()?.to_string(), s["pid"].as_u64()))
}

pub fn listening(port: u16) -> bool {
    std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
}

pub struct Killed(pub Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
