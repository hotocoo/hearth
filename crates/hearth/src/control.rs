//! The control socket between `hearthd` and the `hearth` CLI (and so MCP).
//!
//! A Unix socket in hearth's state directory, which is readable by its owner
//! only: who may ask hearthd anything is decided by file permissions, so
//! there is no TCP port to expose and no key to leak. One JSON request per
//! line, one JSON answer per line.

use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

/// Where hearthd listens.
pub fn socket_path() -> Result<PathBuf, String> {
    hearth_core::state::dir()
        .map(|d| d.join("hearthd.sock"))
        .map_err(|e| e.to_string())
}

/// Explains an error from binding or connecting to the socket. The one
/// that needs explaining: socket paths have a short length limit (about a
/// hundred bytes), which a deep state directory exceeds.
pub fn explain(path: &std::path::Path, e: &std::io::Error) -> String {
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

/// Sends one request to hearthd and returns its answer. hearthd answers
/// from memory, so the wait is bounded by the first-contact deadline.
pub fn call(request: &Value) -> Result<Value, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let path = socket_path()?;
    let mut stream = UnixStream::connect(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::InvalidInput => explain(&path, &e),
        _ => format!(
            "hearthd is not running ({}: {e}); start it with `hearthd`",
            path.display()
        ),
    })?;
    let deadline = Some(Duration::from_millis(
        config.discovery.first_contact_timeout_ms,
    ));
    stream
        .set_read_timeout(deadline)
        .and_then(|()| stream.set_write_timeout(deadline))
        .and_then(|()| writeln!(stream, "{request}"))
        .map_err(|e| format!("hearthd: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("hearthd did not answer: {e}"))?;
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
        let last = s["last_event"]["event"].as_str().unwrap_or("-");
        let _ = writeln!(
            out,
            "{:<16} {:<22} {:<9} pid {:<7} restarts {:<3} last: {last}",
            s["name"].as_str().unwrap_or("?"),
            s["addr"].as_str().unwrap_or("?"),
            s["state"].as_str().unwrap_or("?"),
            s["pid"].as_u64().map_or("-".into(), |p| p.to_string()),
            s["restarts"].as_u64().unwrap_or_default(),
        );
    }
    Ok(out)
}

pub fn restart(args: &RestartArgs, json: bool) -> Result<String, String> {
    let answer = call(&json!({ "op": "restart", "server": args.server }))?;
    if json {
        crate::to_json(&answer)
    } else {
        Ok(format!(
            "restart of {} requested; `hearth status` shows it come back\n",
            args.server
        ))
    }
}
