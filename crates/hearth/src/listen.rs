//! The accept loop hearthd's TCP endpoints share (metrics, gateway,
//! dashboard): a thread per connection, up to `max_clients` at once, so a
//! stalled client cannot hold the others up.

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
                if let Err(e) = handle(stream) {
                    println!("{what}: {e}");
                }
                done.fetch_sub(1, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            active.fetch_sub(1, Ordering::SeqCst);
            println!("{what}: no thread for a connection: {e}");
        }
    }
}
