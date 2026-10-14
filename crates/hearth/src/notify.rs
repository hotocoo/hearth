//! Telling a person when something needs them: a desktop notification
//! and/or a webhook, both from configuration and both off by default.
//!
//! Delivery uses the platform's own tools, so hearth ships no TLS stack or
//! notification library: `osascript` on macOS, `notify-send` on Linux, and
//! `curl` for the webhook (any `https://` service works). Every attempt is
//! bounded by `notify.timeout_ms`, runs on its own thread, and reports a
//! failure on stderr; it never fails or slows the command that noticed.
//!
//! The webhook URL often carries a token, so it goes to curl on stdin, in a
//! curl config, never on the command line where `ps` shows it.

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
    let timeout = Duration::from_millis(config.timeout_ms);
    if config.desktop {
        let command = desktop(title, message);
        sending.push(std::thread::spawn(move || {
            report(
                "desktop notification",
                command.and_then(|c| run(c, None, timeout)),
            );
        }));
    }
    if let Some(url) = config.webhook_url.clone() {
        let curl_config = format!(
            "url = \"{}\"\nheader = \"Content-Type: application/json\"\ndata-binary = \"{}\"\n",
            quoted(&url),
            quoted(&body.to_string())
        );
        sending.push(std::thread::spawn(move || {
            let mut c = Command::new("curl");
            // -q first: ignore ~/.curlrc. --globoff: braces and brackets in
            // a URL are literal. -K -: the rest comes from stdin.
            c.args(["-q", "--globoff", "--silent", "--show-error", "--fail"])
                .arg("--max-time")
                .arg(format!("{:.3}", timeout.as_secs_f64()))
                .args(["-K", "-"]);
            report("webhook", run(c, Some(curl_config), timeout));
        }));
    }
    sending
}

/// Escapes text for a double-quoted curl config value.
fn quoted(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
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

/// Runs `c` with `input` on stdin, killing it if it has not finished
/// within `timeout` (a desktop notifier can block on a dead session bus).
fn run(mut c: Command, input: Option<String>, timeout: Duration) -> Result<(), String> {
    use std::io::{Read as _, Write as _};
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
        // A program that exits before reading all of it says why on stderr.
        match stdin.write_all(text.as_bytes()) {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(e));
            }
            _ => {}
        }
    }
    // stderr reaches end of file when the program exits, so reading it on
    // another thread is the wait, and the channel gives it a deadline.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            let _ = tx.send(text);
        });
    }
    let Ok(stderr) = rx.recv_timeout(timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "{program} did not finish within {timeout:?}; killed"
        ));
    };
    let status = child.wait().map_err(fail)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {status}: {}", stderr.trim()))
    }
}

fn report(what: &str, result: Result<(), String>) {
    if let Err(e) = result {
        eprintln!("hearth: {what} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curl_config_values_are_escaped() {
        assert_eq!(quoted(r#"{"a":"x\"y"}"#), r#"{\"a\":\"x\\\"y\"}"#);
    }
}
