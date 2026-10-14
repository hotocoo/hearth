//! A deliberately small HTTP/1.1 GET for probing local ports.
//!
//! Probes go to sockets on this machine that may not speak HTTP at all, or may
//! accept a connection and never answer. Everything is bounded by the caller's
//! deadline and byte limit, and a non-HTTP reply is reported as such rather
//! than retried.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    pub elapsed: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// Nothing accepted the connection.
    Refused,
    /// Connected, but no complete answer before the deadline.
    NoAnswer {
        after: Duration,
    },
    /// Something answered, but not with HTTP.
    NotHttp,
    /// The answer was larger than the caller allows.
    TooLarge,
    Io(String),
}

pub fn get(
    addr: SocketAddr,
    path: &str,
    deadline: Duration,
    max_bytes: u64,
) -> Result<Response, ProbeError> {
    let started = Instant::now();
    let remaining = || {
        deadline
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
    };
    let mut stream = TcpStream::connect_timeout(&addr, deadline).map_err(|e| match e.kind() {
        std::io::ErrorKind::ConnectionRefused => ProbeError::Refused,
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ProbeError::NoAnswer {
            after: started.elapsed(),
        },
        _ => ProbeError::Io(e.to_string()),
    })?;
    let host = match addr {
        SocketAddr::V4(a) => format!("{}:{}", a.ip(), a.port()),
        SocketAddr::V6(a) => format!("[{}]:{}", a.ip(), a.port()),
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: application/json\r\nUser-Agent: hearth\r\nConnection: close\r\n\r\n"
    );
    stream
        .set_write_timeout(remaining())
        .and_then(|()| stream.write_all(request.as_bytes()))
        .map_err(|e| ProbeError::Io(e.to_string()))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let limit = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    loop {
        let Some(left) = remaining() else {
            return Err(ProbeError::NoAnswer {
                after: started.elapsed(),
            });
        };
        stream
            .set_read_timeout(Some(left))
            .map_err(|e| ProbeError::Io(e.to_string()))?;
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if !raw.is_empty() && !raw.starts_with(b"HTTP/"[..raw.len().min(5)].as_ref()) {
                    return Err(ProbeError::NotHttp);
                }
                if raw.len() > limit {
                    return Err(ProbeError::TooLarge);
                }
                if let Some(done) = complete(&raw) {
                    return done.map(|(status, body)| Response {
                        status,
                        body,
                        elapsed: started.elapsed(),
                    });
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(ProbeError::NoAnswer {
                    after: started.elapsed(),
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset && raw.is_empty() => {
                return Err(ProbeError::NotHttp);
            }
            Err(e) => return Err(ProbeError::Io(e.to_string())),
        }
    }
    match complete_at_eof(&raw) {
        Some((status, body)) => Ok(Response {
            status,
            body,
            elapsed: started.elapsed(),
        }),
        None if raw.is_empty() => Err(ProbeError::NotHttp),
        None => Err(ProbeError::NotHttp),
    }
}

struct Head {
    status: u16,
    body_start: usize,
    content_length: Option<usize>,
    chunked: bool,
}

fn head(raw: &[u8]) -> Option<Result<Head, ProbeError>> {
    let end = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let text = String::from_utf8_lossy(&raw[..end]);
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok());
    let Some(status) = status else {
        return Some(Err(ProbeError::NotHttp));
    };
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().ok();
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        }
    }
    Some(Ok(Head {
        status,
        body_start: end + 4,
        content_length,
        chunked,
    }))
}

/// `Some` once the response is complete without waiting for EOF.
fn complete(raw: &[u8]) -> Option<Result<(u16, Vec<u8>), ProbeError>> {
    let h = match head(raw)? {
        Ok(h) => h,
        Err(e) => return Some(Err(e)),
    };
    let body = &raw[h.body_start..];
    if h.chunked {
        return dechunk(body).map(|b| Ok((h.status, b)));
    }
    let len = h.content_length?;
    (body.len() >= len).then(|| Ok((h.status, body[..len].to_vec())))
}

fn complete_at_eof(raw: &[u8]) -> Option<(u16, Vec<u8>)> {
    let h = head(raw)?.ok()?;
    let body = &raw[h.body_start..];
    if h.chunked {
        return dechunk(body).map(|b| (h.status, b));
    }
    Some((h.status, body.to_vec()))
}

/// The decoded body once the terminating zero-length chunk has arrived.
fn dechunk(mut body: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&body[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if body.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(reply: &'static [u8], delay: Option<Duration>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            if let Some(d) = delay {
                thread::sleep(d);
            }
            let _ = s.write_all(reply);
        });
        addr
    }

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn content_length_and_chunked_bodies() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"version\":1}",
            None,
        );
        let r = get(addr, "/", SECOND, 1 << 20).unwrap();
        assert_eq!(
            (r.status, r.body.as_slice()),
            (200, b"{\"version\":1}".as_slice())
        );

        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n",
            None,
        );
        let r = get(addr, "/", SECOND, 1 << 20).unwrap();
        assert_eq!(r.body, b"{\"a\":1}");
    }

    #[test]
    fn silent_server_is_no_answer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Accepts (the kernel completes the handshake) but never replies.
        let err = get(addr, "/", Duration::from_millis(150), 1 << 20).unwrap_err();
        assert!(matches!(err, ProbeError::NoAnswer { .. }));
        drop(listener);
    }

    #[test]
    fn non_http_and_refused_and_oversized() {
        let addr = serve_once(b"-ERR unknown command\r\n", None);
        assert_eq!(
            get(addr, "/", SECOND, 1 << 20).unwrap_err(),
            ProbeError::NotHttp
        );

        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert_eq!(
            get(closed, "/", SECOND, 1 << 20).unwrap_err(),
            ProbeError::Refused
        );

        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789",
            None,
        );
        assert_eq!(
            get(addr, "/", SECOND, 16).unwrap_err(),
            ProbeError::TooLarge
        );
    }
}
