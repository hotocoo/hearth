//! hearthd starts one server at a time (`supervise.serialize_loads`): with
//! two servers that each take a while to come up, the second starts only
//! once the first is ready.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::process::Stdio;

mod common;
use common::*;

/// How long each fake server takes to start listening, in seconds.
const DELAY_S: &str = "1.5";

#[test]
fn hearthd_starts_one_server_at_a_time() {
    if !python() {
        eprintln!("skipped: python3 is needed for the fake servers");
        return;
    }
    let dir = home("loads");
    let script = dir.join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    let (a, b) = (free_port(), free_port());
    let server = |name: &str, port: u16| {
        format!(
            "[servers.{name}]\nport = {port}\ncommand = [\"python3\", \"{}\", \"{port}\"]\nenv = {{ E2E_DELAY = \"{DELAY_S}\" }}\n",
            script.display()
        )
    };
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "[watch]\nin_hearthd = false\n{}{}",
            server("one", a),
            server("two", b)
        ),
    )
    .unwrap();
    let _daemon = Killed(hearthd(&dir).stdout(Stdio::null()).spawn().unwrap());
    wait_until("both ready", || {
        hearth(&dir, &["status"]).is_ok_and(|v| {
            v["servers"]
                .as_array()
                .is_some_and(|s| s.len() == 2 && s.iter().all(|s| s["state"] == "ready"))
        })
    });
    // The run journal, oldest first: each server's start and ready.
    let log = hearth(&dir, &["log", "--kind", "run", "--limit", "20"]).unwrap();
    let mut events: Vec<(u64, String)> = log["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["taken_at_ms"].as_u64().unwrap(),
                s["summary"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    events.sort();
    let at = |port: u16, what: &str| {
        events
            .iter()
            .find(|(_, s)| s.contains(&format!(":{port} {what}")))
            .unwrap_or_else(|| panic!("no {what} for {port} in {events:?}"))
            .0
    };
    let (first, second) = if at(a, "started") < at(b, "started") {
        (a, b)
    } else {
        (b, a)
    };
    assert!(
        at(second, "started") >= at(first, "ready"),
        "the second server started before the first was ready: {events:?}"
    );
}

/// A server is held from starting while its last measured footprint plus
/// the headroom does not fit; a server never measured starts.
#[test]
fn hearthd_holds_a_start_its_last_footprint_says_would_not_fit() {
    if !python() {
        eprintln!("skipped: python3 is needed for the fake server");
        return;
    }
    let dir = home("admit");
    let script = dir.join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    let port = free_port();
    // A headroom no machine has: once measured, the server can never fit.
    std::fs::write(
        dir.join("hearth.toml"),
        format!(
            "[watch]\nin_hearthd = false\n[supervise]\nadmit_headroom_bytes = 1125899906842624\n[servers.big]\nport = {port}\ncommand = [\"python3\", \"{}\", \"{port}\"]\n",
            script.display()
        ),
    )
    .unwrap();
    let _daemon = Killed(hearthd(&dir).stdout(Stdio::null()).spawn().unwrap());
    let server = || {
        hearth(&dir, &["status"])
            .ok()
            .map(|v| v["servers"][0].clone())
    };
    wait_until("the first start", || {
        server().is_some_and(|s| s["state"] == "ready")
    });
    hearth(&dir, &["restart", "big"]).unwrap();
    wait_until("the restart to be held", || {
        server().is_some_and(|s| {
            s["held"]
                .as_str()
                .is_some_and(|h| h.contains("its last start held"))
        })
    });
    assert_ne!(server().unwrap()["state"], "ready");
    assert!(!listening(port), "a held server must not be running");
}
