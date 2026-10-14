//! Telling a person when something needs them: a desktop notification
//! and/or a webhook, both from configuration and both off by default.
//!
//! Delivery uses the platform's own tools, so hearth ships no TLS stack or
//! notification library: `osascript` on macOS, `notify-send` on Linux, and
//! `curl` for the webhook (any `https://` service works), bounded by
//! `notify.timeout_ms`. Each attempt runs on its own thread and a failure is
//! reported on stderr; it never fails or slows the command that noticed.

use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::Duration;

use hearth_core::config::Notify;
use serde_json::Value;

/// Sends `title`/`message` to the desktop and `body` to the webhook, as
/// configured. Join the returned handles before exiting, or a notification
/// sent just before the end is lost with the process.
#[must_use]
pub fn send(config: &Notify, title: &str, message: &str, body: &Value) -> Vec<JoinHandle<()>> {
    let mut sending = Vec::new();
    if config.desktop {
        let command = desktop(title, message);
        sending.push(std::thread::spawn(move || {
            report("desktop notification", command.and_then(|c| run(c, None)));
        }));
    }
    if let Some(url) = config.webhook_url.clone() {
        let payload = body.to_string();
        let timeout = Duration::from_millis(config.timeout_ms);
        sending.push(std::thread::spawn(move || {
            let mut c = Command::new("curl");
            c.args(["--silent", "--show-error", "--fail", "--max-time"])
                .arg(format!("{:.3}", timeout.as_secs_f64()))
                .args([
                    "--header",
                    "Content-Type: application/json",
                    "--data-binary",
                    "@-",
                ])
                .arg(&url);
            report("webhook", run(c, Some(payload)));
        }));
    }
    sending
}

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)] // the same signature as platforms without one
fn desktop(title: &str, message: &str) -> Result<Command, String> {
    // Text goes in as arguments, never into the script, so nothing in it
    // can be read as AppleScript.
    let mut c = Command::new("osascript");
    c.args([
        "-e",
        "on run argv",
        "-e",
        "display notification (item 2 of argv) with title (item 1 of argv)",
        "-e",
        "end run",
        title,
        message,
    ]);
    Ok(c)
}

#[cfg(all(unix, not(target_os = "macos")))]
#[allow(clippy::unnecessary_wraps)] // the same signature as platforms without one
fn desktop(title: &str, message: &str) -> Result<Command, String> {
    let mut c = Command::new("notify-send");
    c.args(["--app-name=hearth", "--", title, message]);
    Ok(c)
}

#[cfg(not(unix))]
fn desktop(_title: &str, _message: &str) -> Result<Command, String> {
    Err("desktop notifications are not supported on this platform".into())
}

/// Runs `c` with `input` on stdin until it exits. Desktop tools return at
/// once; `curl` is bounded by its own `--max-time`.
fn run(mut c: Command, input: Option<String>) -> Result<(), String> {
    use std::io::Write as _;
    let program = c.get_program().to_string_lossy().into_owned();
    let fail = |e: std::io::Error| format!("{program}: {e}");
    let mut child = c
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(fail)?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes()).map_err(fail)?;
    }
    let out = child.wait_with_output().map_err(fail)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn report(what: &str, result: Result<(), String>) {
    if let Err(e) = result {
        eprintln!("hearth: {what} failed: {e}");
    }
}
