//! Retry before the first byte: a request whose body is small enough to
//! keep is sent again when its server dies, resets the connection or fails
//! with a 5xx that hearthd then restarts it for, as long as nothing has
//! reached the client yet. Once a byte is sent, the answer is the client's
//! and is never replayed.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use hearth_core::config::Gateway;
use serde_json::Value;

use super::{Forward, Refusal, Servers, refuse};

/// The body length a head declares, when it can be kept for a retry: a
/// `Content-Length` within `max`, and no chunked transfer.
pub(super) fn keepable_length(head: &str, max: u64) -> Option<u64> {
    let mut length = Some(0);
    for line in head.lines().skip(1) {
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        match field.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" => return None,
            "content-length" => length = value.trim().parse().ok(),
            _ => {}
        }
    }
    length.filter(|&n| n <= max)
}

/// Reads the `length`-byte body the client is sending.
pub(super) fn read_body(
    client: &mut TcpStream,
    length: u64,
    within: Duration,
) -> std::io::Result<Vec<u8>> {
    client.set_read_timeout(Some(within))?;
    let mut body = vec![0; usize::try_from(length).unwrap_or(usize::MAX)];
    client.read_exact(&mut body)?;
    client.set_read_timeout(None)?;
    Ok(body)
}

/// The status code at the start of an answer, when it has arrived.
fn status(first: &[u8]) -> Option<u16> {
    let line = std::str::from_utf8(first.get(..12)?).ok()?;
    line.strip_prefix("HTTP/1.")?.get(2..5)?.parse().ok()
}

/// One attempt's outcome.
pub(super) enum Attempt {
    /// The server answered; its first bytes, and the connection to read the
    /// rest from.
    Answered(TcpStream, Vec<u8>),
    /// Nothing usable came back and the server may come back: try again.
    Retry(String),
    /// The server is silent past `idle`: a wedge is hearthd's to restart,
    /// and waiting again would double the client's wait.
    Silent,
}

/// Sends the request once and reads the first bytes of the answer.
pub(super) fn attempt(forward: &Forward, body: &[u8], config: &Gateway) -> Attempt {
    let connect = Duration::from_millis(config.connect_timeout_ms);
    let mut upstream = match TcpStream::connect_timeout(&forward.addr, connect) {
        Ok(s) => s,
        Err(e) => return Attempt::Retry(format!("connect: {e}")),
    };
    let sent = upstream
        .set_read_timeout(Some(Duration::from_millis(config.idle_timeout_ms)))
        .and_then(|()| upstream.write_all(forward.head.as_bytes()))
        .and_then(|()| upstream.write_all(body));
    if let Err(e) = sent {
        return Attempt::Retry(format!("send: {e}"));
    }
    let mut buf = vec![0u8; 16 * 1024];
    match upstream.read(&mut buf) {
        Ok(0) => Attempt::Retry("closed before answering".into()),
        Ok(n) => {
            buf.truncate(n);
            Attempt::Answered(upstream, buf)
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Attempt::Silent
        }
        Err(e) => Attempt::Retry(format!("read: {e}")),
    }
}

/// The server's pid as hearthd reports it now.
pub(super) fn pid(servers: &Servers, name: &str) -> Option<u64> {
    identity(servers, name).1
}

fn identity(servers: &Servers, name: &str) -> (Option<String>, Option<u64>) {
    servers()
        .iter()
        .find(|s| s["name"] == name)
        .map_or((None, None), |s: &Value| {
            (s["state"].as_str().map(String::from), s["pid"].as_u64())
        })
}

/// Whether hearthd, within `retry_grace_ms`, sees the server it reported
/// with pid `before` leave `ready` or come back with a new pid: it noticed
/// the failure and is restarting it. A failure it never notices is not
/// one a retry can fix.
pub(super) fn hearthd_restarts(
    servers: &Servers,
    name: &str,
    before: Option<u64>,
    config: &Gateway,
) -> bool {
    let until = Instant::now() + Duration::from_millis(config.retry_grace_ms);
    loop {
        let (state, now) = identity(servers, name);
        if state.as_deref() != Some("ready") || now != before {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(config.ready_poll_ms));
    }
}

/// Whether the first bytes are a 5xx status.
pub(super) fn server_error(first: &[u8]) -> bool {
    status(first).is_some_and(|s| (500..600).contains(&s))
}

/// The refusal when every attempt failed.
pub(super) fn exhausted(forward: &Forward, tries: u64, last: &str) -> Refusal {
    refuse(
        "502 Bad Gateway",
        "unreachable",
        format!(
            "server {} failed {tries} attempt(s); last: {last}",
            forward.name
        ),
        format!("hearth status {0}; hearth logs {0}", forward.name),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_small_declared_bodies_are_kept() {
        let head = |h: &str| format!("POST /s/a/v1 HTTP/1.1\r\nHost: x\r\n{h}\r\n");
        assert_eq!(keepable_length(&head(""), 10), Some(0));
        assert_eq!(keepable_length(&head("Content-Length: 7\r\n"), 10), Some(7));
        assert_eq!(keepable_length(&head("content-length: 11\r\n"), 10), None);
        assert_eq!(
            keepable_length(&head("Transfer-Encoding: chunked\r\n"), 10),
            None
        );
        assert_eq!(keepable_length(&head("Content-Length: x\r\n"), 10), None);
    }

    #[test]
    fn status_codes_are_read_from_the_first_bytes() {
        assert_eq!(status(b"HTTP/1.1 503 Service Unavailable\r\n"), Some(503));
        assert_eq!(status(b"HTTP/1.0 200 OK\r\n"), Some(200));
        assert_eq!(status(b"HTTP/1.1"), None);
        assert_eq!(status(b"garbage here!"), None);
    }
}
