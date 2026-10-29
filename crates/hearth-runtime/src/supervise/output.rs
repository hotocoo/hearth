//! A server's output, read line by line: kept for `hearth logs`, echoed
//! when asked, and searched for the signals and causes its runtime logs.

use std::io::{BufRead as _, BufReader, Read};
use std::sync::mpsc::Sender;

use super::{Output, StartLog, said};
use crate::llama_log::compute_bytes;
use crate::signals::{Signal, cause, classify};

/// Passes the server's output on to `echo` (hearth's stderr) line by line,
/// and sends every line that carries a signal. Ends when the server's group closes the
/// stream.
pub(super) fn watch<R: Read + Send + 'static>(
    stream: R,
    signals: Sender<(Signal, String)>,
    (output, keep, line_bytes, log): (Output, usize, usize, StartLog),
    mut echo: Option<Box<dyn std::io::Write + Send>>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            // Memory stays bounded however the server writes: a line longer
            // than the cap arrives in pieces of that size.
            match (&mut reader)
                .take(cap(line_bytes))
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => {
                    said(&log).open -= 1;
                    log.1.notify_all();
                    return;
                }
                Ok(_) => {}
            }
            // A closed stderr leaves the log unseen, never the server unwatched.
            if let Some(echo) = &mut echo {
                let _ = echo.write_all(&line);
            }
            let text = String::from_utf8_lossy(&line);
            {
                let mut kept = output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                kept.push_back(cut(text.trim_end(), line_bytes));
                while kept.len() > keep {
                    kept.pop_front();
                }
            }
            {
                let mut s = said(&log);
                s.lines += 1;
                if let Some(bytes) = compute_bytes(&text) {
                    s.compute_bytes = s.compute_bytes.saturating_add(bytes);
                }
                // The first of a burst explains it; one long past does not.
                let stale = s
                    .cause
                    .as_ref()
                    .is_none_or(|(_, _, at)| s.lines - at > keep);
                if stale && let Some(c) = cause(&text) {
                    let at = s.lines;
                    s.cause = Some((c, cut(text.trim_end(), line_bytes), at));
                }
            }
            if let Some(signal) = classify(&text) {
                // Fails only once the supervisor has moved on to another
                // server; this one's output is still passed on until it closes.
                let _ = signals.send((signal, text.trim_end().to_string()));
            }
        }
    });
}

/// The most read as one piece of a line: twice the kept length, so a
/// signature that starts near the end of the kept part is still seen whole.
fn cap(line_bytes: usize) -> u64 {
    u64::try_from(line_bytes.saturating_mul(2)).unwrap_or(u64::MAX)
}

/// `text` cut to at most `max` bytes, on a character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} [cut]", &text[..end])
}
