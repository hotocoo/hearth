//! The accept loop hearthd's TCP endpoints share (metrics, gateway,
//! dashboard): a thread per connection, up to `max_clients` at once, so a
//! stalled client cannot hold the others up.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::Instant;

/// Serves `listener` until hearthd ends. `handle` answers one connection;
/// `busy` answers one beyond `max_clients` (or drops it), never waiting.
/// After an accept error (out of descriptors, typically) it pauses for
/// `pause` rather than spin.
pub fn serve(
    what: &'static str,
    listener: &TcpListener,
    max_clients: u64,
    pause: Duration,
    handle: &Arc<dyn Fn(TcpStream) -> std::io::Result<()> + Send + Sync>,
    busy: impl Fn(TcpStream),
) {
    let active = Arc::new(AtomicU64::new(0));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                println!("{what}: {e}");
                std::thread::sleep(pause);
                continue;
            }
        };
        if active.fetch_add(1, Ordering::SeqCst) >= max_clients {
            active.fetch_sub(1, Ordering::SeqCst);
            busy(stream);
            continue;
        }
        let (handle, done) = (handle.clone(), active.clone());
        let spawned = std::thread::Builder::new()
            .name(what.into())
            .spawn(move || {
                // A handler that panics still gives its slot back.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(stream))) {
                    Ok(Err(e)) => println!("{what}: {e}"),
                    Err(_) => println!("{what}: a connection's handler panicked"),
                    Ok(Ok(())) => {}
                }
                done.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            active.fetch_sub(1, Ordering::SeqCst);
            println!("{what}: no thread for a connection: {e}");
        }
    }
}

/// The request head (up to the blank line), read within `max` bytes and
/// before `deadline` in total: each read waits at most the time left when
/// reading began, and the deadline is checked after every byte, so a client
/// sending a byte at a time cannot hold the handler.
pub fn read_head(stream: &TcpStream, max: u64, deadline: Instant) -> std::io::Result<String> {
    use std::io::{Error, ErrorKind, Read as _};
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| Error::new(ErrorKind::TimedOut, "request took too long"))
    };
    stream.set_read_timeout(Some(left()?))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    let mut limited = stream.take(max);
    while !head.ends_with(b"\r\n\r\n") && !head.ends_with(b"\n\n") {
        if limited.read(&mut byte)? == 0 {
            break;
        }
        head.push(byte[0]);
        left()?;
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Writes one whole answer with `Connection: close`; `extra` is any further
/// header lines, each ending in CRLF.
pub fn respond(
    mut stream: impl std::io::Write,
    status: &str,
    kind: &str,
    extra: &str,
    body: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
        body.len()
    )
}
