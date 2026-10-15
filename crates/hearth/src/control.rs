//! The control socket between `hearthd` and the `hearth` CLI (and so MCP).
//!
//! A Unix socket in hearth's state directory. Who may talk to hearthd is
//! decided by ownership and permissions, checked on both ends: the
//! directory must belong to this user and admit nobody else, and so must the
//! socket. There is no TCP port to expose and no key to leak. One JSON
//! request per line and one JSON answer per line, each at most
//! `control.max_message_bytes`, the whole exchange within
//! `control.timeout_ms`.

use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Permission bits that let anyone but the owner in.
const GROUP_OR_OTHER: u32 = 0o077;

/// Where hearthd listens.
pub fn socket_path() -> Result<PathBuf, String> {
    hearth_core::state::dir()
        .map(|d| d.join("hearthd.sock"))
        .map_err(|e| e.to_string())
}

/// Explains an error from binding or connecting to the socket. The one
/// that needs explaining: socket paths have a short length limit (about a
/// hundred bytes), which a deep state directory exceeds.
pub fn explain(path: &Path, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::InvalidInput {
        format!(
            "{}: {e}; the path is {} bytes, too long for a socket: set HEARTH_STATE to a shorter directory",
            path.display(),
            path.as_os_str().len()
        )
    } else {
        format!("{}: {e}", path.display())
    }
}

/// `path` must be what it claims (not a symlink to something else), belong
/// to this user, and admit nobody else.
pub fn private(path: &Path, want_socket: bool) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let kind_ok = if want_socket {
        meta.file_type().is_socket()
    } else {
        meta.is_dir()
    };
    if !kind_ok {
        return Err(format!(
            "{} is not a {}; refusing to use it",
            path.display(),
            if want_socket { "socket" } else { "directory" }
        ));
    }
    if meta.uid() != hearth_sense::effective_uid() {
        return Err(format!(
            "{} belongs to another user; refusing to use it",
            path.display()
        ));
    }
    if meta.mode() & GROUP_OR_OTHER != 0 {
        return Err(format!(
            "{} is open to other users (mode {:o}); run chmod go= on it, or set HEARTH_STATE to a private directory",
            path.display(),
            meta.mode() & 0o7777
        ));
    }
    Ok(())
}

fn left(deadline: Instant) -> Result<Duration, String> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "timed out".into())
}

/// Reads one line, at most `max` bytes, before `deadline`. Each read waits
/// at most the time left when reading began, and the deadline is checked
/// after every byte, so a peer sending a byte at a time cannot stretch the
/// exchange past the deadline by more than one such wait. (The socket
/// timeout is set once: macOS refuses to change it once the peer has closed,
/// which is exactly when the last bytes are still being read.)
pub fn read_line(stream: &UnixStream, max: u64, deadline: Instant) -> Result<String, String> {
    stream
        .set_read_timeout(Some(left(deadline)?))
        .map_err(|e| e.to_string())?;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    let mut limited = stream.take(max);
    loop {
        match limited.read(&mut byte) {
            Ok(0) if limited.limit() == 0 => return Err(format!("longer than {max} bytes")),
            Ok(0) => return Err("closed before a full line".into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => line.push(byte[0]),
            Err(e) => return Err(e.to_string()),
        }
        left(deadline)?;
    }
    String::from_utf8(line).map_err(|_| "not UTF-8".into())
}

/// Writes one line before `deadline`.
pub fn write_line(mut stream: &UnixStream, line: &str, deadline: Instant) -> Result<(), String> {
    stream
        .set_write_timeout(Some(left(deadline)?))
        .and_then(|()| writeln!(stream, "{line}"))
        .map_err(|e| e.to_string())
}

/// Sends one request to hearthd and returns its answer.
pub fn call(request: &Value) -> Result<Value, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let path = socket_path()?;
    let deadline = Instant::now() + Duration::from_millis(config.control.timeout_ms);
    let max = config.control.max_message_bytes;
    let stream = UnixStream::connect(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidInput => explain(&path, &e),
        _ => format!(
            "hearthd is not running ({}: {e}); start it with `hearthd`",
            path.display()
        ),
    })?;
    // Before trusting an answer, make sure it can only come from this user.
    if let Some(dir) = path.parent() {
        private(dir, false)?;
    }
    private(&path, true)?;
    write_line(&stream, &request.to_string(), deadline).map_err(|e| format!("hearthd: {e}"))?;
    let line = read_line(&stream, max, deadline).map_err(|e| format!("hearthd: {e}"))?;
    let answer: Value = serde_json::from_str(&line).map_err(|e| format!("hearthd: {e}"))?;
    match answer["error"].as_str() {
        Some(e) => Err(e.to_string()),
        None => Ok(answer),
    }
}

#[derive(clap::Args)]
pub struct StatusArgs {}

#[derive(clap::Args)]
pub struct RestartArgs {
    /// Name of the server, as in `[servers.NAME]`.
    server: String,
}

#[derive(clap::Args)]
pub struct LogsArgs {
    /// Name of the server, as in `[servers.NAME]`.
    server: String,
}

/// Text from hearthd without control characters, so nothing it says can
/// steer the terminal.
pub fn printable(v: &Value) -> String {
    v.as_str()
        .unwrap_or("-")
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

pub fn status(_: &StatusArgs, json: bool) -> Result<String, String> {
    let answer = call(&json!({ "op": "status" }))?;
    if json {
        return crate::to_json(&answer);
    }
    let mut out = String::new();
    let servers = answer["servers"].as_array().cloned().unwrap_or_default();
    if servers.is_empty() {
        out.push_str("hearthd runs no servers: add [servers.NAME] to your config\n");
    }
    for s in servers {
        let _ = writeln!(out, "{}", line(&s));
    }
    Ok(out)
}

/// One server as `hearth status` and `hearth top` show it.
pub fn line(s: &Value) -> String {
    format!(
        "{:<16} {:<22} {:<9} pid {:<7} {:>11} restarts {:<3} last: {}",
        printable(&s["name"]),
        printable(&s["addr"]),
        printable(&s["state"]),
        s["server_pid"]
            .as_u64()
            .or_else(|| s["pid"].as_u64())
            .map_or("-".into(), |p| p.to_string()),
        s["footprint_bytes"]
            .as_u64()
            .map_or(String::new(), crate::units::bytes),
        s["restarts"].as_u64().unwrap_or_default(),
        printable(&s["last_event"]["event"]),
    ) + &s["held"].as_str().map_or(String::new(), |h| {
        format!(" · start held: {}", printable(&Value::from(h)))
    })
}

/// Sends `op` (`restart`, `stop` or `start`) for one server.
pub fn order(op: &str, args: &RestartArgs, json: bool) -> Result<String, String> {
    let answer = call(&json!({ "op": op, "server": args.server }))?;
    if json {
        return crate::to_json(&answer);
    }
    Ok(match op {
        "stop" => format!(
            "stop of {} requested; it stays down until `hearth start {}`\n",
            args.server, args.server
        ),
        "start" => format!(
            "start of {} requested; `hearth status` shows it come up\n",
            args.server
        ),
        _ => format!(
            "restart of {} requested; `hearth status` shows it come back\n",
            args.server
        ),
    })
}

/// The server's last lines of output, as hearthd kept them.
pub fn logs(args: &LogsArgs, json: bool) -> Result<String, String> {
    let answer = call(&json!({ "op": "logs", "server": args.server }))?;
    if json {
        return crate::to_json(&answer);
    }
    let mut out = String::new();
    for line in answer["lines"].as_array().into_iter().flatten() {
        let _ = writeln!(out, "{}", printable(line));
    }
    Ok(out)
}

#[derive(clap::Args)]
pub struct ReloadArgs {}

/// Asks hearthd to apply the configuration's `[servers]` now.
pub fn reload(_: &ReloadArgs, json: bool) -> Result<String, String> {
    let answer = call(&json!({ "op": "reload" }))?;
    if json {
        return crate::to_json(&answer);
    }
    let list = |k: &str| {
        let names: Vec<String> = answer[k]
            .as_array()
            .into_iter()
            .flatten()
            .map(printable)
            .collect();
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    };
    Ok(format!(
        "reloading: added {}; removed {}; changed {}. The rest keep running; hearth status shows the result\n",
        list("added"),
        list("removed"),
        list("changed")
    ))
}
