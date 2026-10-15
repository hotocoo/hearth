//! The journal's view of `hearth ports` snapshots: one line per snapshot,
//! the entries `follow` tracks, and what changed between two.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use hearth_runtime::fingerprint::Identity;
use serde::Deserialize as _;
use serde_json::Value;

use super::{Key, describe, is_runtime, program};
use crate::history::{Change, Delta, Unit};
use crate::units::bytes;

/// The ports in a recorded report, keyed by program and port, each with
/// every pid listening there (prefork servers and `SO_REUSEPORT` share one).
/// Read field by field so snapshots from other hearth versions still compare.
pub(super) fn recorded(body: &Value) -> BTreeMap<Key, BTreeMap<i64, &Value>> {
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
pub(super) const NO_ANSWER: &str = "NO ANSWER";

/// What a recorded port looked like, without the parts that differ on every
/// probe (how long a silent port was waited for).
pub fn state_of(port: &Value) -> String {
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

pub fn pids(by_pid: &BTreeMap<i64, &Value>) -> String {
    let list: Vec<String> = by_pid.keys().map(ToString::to_string).collect();
    format!("pid {}", list.join(","))
}

/// The distinct states of the processes on one port, in pid order.
pub fn states(by_pid: &BTreeMap<i64, &Value>) -> String {
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
