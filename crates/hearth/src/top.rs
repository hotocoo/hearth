//! `hearth top`: one screen that keeps showing what hearthd runs, how the
//! machine holds up and what the journal recorded, and takes keys to
//! restart, stop and start servers and read their output. Like `top`, it
//! redraws until `q`.
//!
//! Everything shown comes from what other commands already measure:
//! `hearth status`, `hearth doctor` (sampled without recording, so a
//! running `top` does not fill the journal) and `hearth log --all`. Agents
//! get the same frame from `hearth --json top`; what they do through
//! `hearth restart` or `hearth note` shows up here on the next refresh.
//!
//! The terminal is driven with escape sequences and `stty`, so no terminal
//! library is needed.

use std::fmt::Write as _;
use std::io::{IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::control::{call, line, printable};

#[derive(clap::Args)]
pub struct Args {
    /// Seconds between refreshes. A key press refreshes at once.
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..))]
    every: u64,
    /// Journal entries to show, newest first.
    #[arg(long, default_value_t = 8)]
    journal: u64,
    /// Print one frame and exit, for scripts (`--json` always does).
    #[arg(long)]
    once: bool,
}

/// What one refresh read. A part that could not be read holds
/// `{"error": ...}`, so the rest still shows.
fn frame(args: &Args) -> Value {
    frame_of(args.journal)
}

/// The frame with the newest `journal` entries; the dashboard reads it too.
pub(crate) fn frame_of(journal: u64) -> Value {
    let or_error = |r: Result<Value, String>| r.unwrap_or_else(|e| json!({ "error": e }));
    let journal = crate::dispatch(&[
        "hearth".into(),
        "--json".into(),
        "log".into(),
        "--all".into(),
        "--limit".into(),
        journal.to_string(),
    ])
    .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()))
    .map(|log| log["snapshots"].clone());
    json!({
        "servers": or_error(call(&json!({ "op": "status" })).map(|a| a["servers"].clone())),
        "machine": or_error(crate::doctor::measure()),
        "journal": or_error(journal),
    })
}

/// Text from anywhere (a note, a server's output) without control
/// characters, so it cannot steer the terminal.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// What the screen shows besides the frame.
#[derive(Default)]
struct View {
    selected: usize,
    /// A server whose output is shown, with its last lines.
    logs: Option<(String, Vec<String>)>,
    /// The answer to the last key.
    message: String,
}

fn section(out: &mut Vec<String>, title: &str) {
    out.push(String::new());
    out.push(format!("── {title}"));
}

/// The frame as lines of text; `keys` adds the selection marker and help.
fn render(frame: &Value, view: &View, keys: bool) -> Vec<String> {
    let mut out = vec![if keys {
        "hearth top · q quit · ↑↓ select · r restart · s stop · S start · l logs".to_string()
    } else {
        "hearth top".to_string()
    }];
    section(&mut out, "servers (hearthd)");
    match frame["servers"].as_array() {
        Some(servers) if servers.is_empty() => {
            out.push("  none: add [servers.NAME] to your config".into());
        }
        Some(servers) => {
            for (i, s) in servers.iter().enumerate() {
                let mark = if keys && i == view.selected { '>' } else { ' ' };
                out.push(format!("{mark} {}", line(s)));
            }
        }
        None => out.push(format!("  {}", printable(&frame["servers"]["error"]))),
    }
    section(&mut out, "machine");
    let machine = &frame["machine"];
    if machine["error"].is_string() {
        out.push(format!("  {}", printable(&machine["error"])));
    } else {
        out.push(format!("  {}", crate::doctor::summary(machine)));
        for f in machine["findings"].as_array().into_iter().flatten() {
            out.push(format!(
                "  [{}] {}",
                printable(&f["severity"]),
                printable(&f["message"])
            ));
        }
    }
    section(&mut out, "journal");
    match frame["journal"].as_array() {
        Some(entries) => {
            for e in entries {
                out.push(format!(
                    "  {:<6} {:>10}  {}",
                    printable(&e["kind"]),
                    crate::history::age(e["taken_at_ms"].as_u64().unwrap_or_default()),
                    printable(&e["summary"])
                ));
            }
        }
        None => out.push(format!("  {}", printable(&frame["journal"]["error"]))),
    }
    if let Some((name, lines)) = &view.logs {
        section(&mut out, &format!("output of {} (l hides)", clean(name)));
        out.extend(lines.iter().map(|l| format!("  {}", clean(l))));
    }
    if !view.message.is_empty() {
        out.push(String::new());
        out.push(clean(&view.message));
    }
    out
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    if json {
        return crate::to_json(&frame(args));
    }
    if args.once {
        return Ok(render(&frame(args), &View::default(), false).join("\n") + "\n");
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("hearth top needs a terminal; `hearth top --once` prints one frame".into());
    }
    let _terminal = Terminal::enter()?;
    interact(args)?;
    Ok(String::new())
}

/// A key, as read from the terminal.
#[derive(Debug, PartialEq, Eq)]
enum Key {
    Quit,
    Up,
    Down,
    Order(&'static str),
    Logs,
    Other,
}

/// The keys in what one read returned.
fn keys(bytes: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while let Some((&b, tail)) = rest.split_first() {
        rest = tail;
        out.push(match b {
            b'q' | 3 => Key::Quit,
            b'k' => Key::Up,
            b'j' => Key::Down,
            b'r' => Key::Order("restart"),
            b's' => Key::Order("stop"),
            b'S' => Key::Order("start"),
            b'l' => Key::Logs,
            0x1b => match rest {
                [b'[', b'A', tail @ ..] => {
                    rest = tail;
                    Key::Up
                }
                [b'[', b'B', tail @ ..] => {
                    rest = tail;
                    Key::Down
                }
                _ => Key::Other,
            },
            _ => Key::Other,
        });
    }
    out
}

fn interact(args: &Args) -> Result<(), String> {
    let mut view = View::default();
    let mut stdin = std::io::stdin().lock();
    loop {
        let frame = frame(args);
        let servers = frame["servers"].as_array().cloned().unwrap_or_default();
        view.selected = view.selected.min(servers.len().saturating_sub(1));
        if let Some((name, _)) = &view.logs {
            view.logs = Some((name.clone(), output(name)));
        }
        draw(&render(&frame, &view, true))?;
        let due = Instant::now() + Duration::from_secs(args.every);
        let mut buf = [0u8; 64];
        // `stty time 1`: each read returns after a key or a tenth of a second.
        while Instant::now() < due {
            let n = stdin.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                continue;
            }
            let name = servers.get(view.selected).map(|s| printable(&s["name"]));
            for key in keys(&buf[..n]) {
                match (key, &name) {
                    (Key::Quit, _) => return Ok(()),
                    (Key::Up, _) => view.selected = view.selected.saturating_sub(1),
                    (Key::Down, _) => view.selected += 1,
                    (Key::Order(op), Some(name)) => {
                        view.message = match call(&json!({ "op": op, "server": name })) {
                            Ok(_) => format!("{op} of {name} requested"),
                            Err(e) => format!("{op} of {name}: {e}"),
                        };
                    }
                    (Key::Logs, Some(name)) => {
                        view.logs = match view.logs.take() {
                            Some((shown, _)) if &shown == name => None,
                            _ => Some((name.clone(), Vec::new())),
                        };
                    }
                    _ => {}
                }
            }
            break;
        }
    }
}

/// A server's last lines, as `hearth logs` prints them.
fn output(name: &str) -> Vec<String> {
    match call(&json!({ "op": "logs", "server": name })) {
        Ok(answer) => answer["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .map(printable)
            .collect(),
        Err(e) => vec![e],
    }
}

/// The terminal in raw mode on the alternate screen, put back as it was
/// when dropped, also on an error or a panic.
struct Terminal {
    saved: String,
}

fn stty(args: &[&str]) -> Result<String, String> {
    let out = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("stty: {e}"))?;
    if !out.status.success() {
        return Err(format!("stty {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Terminal {
    fn enter() -> Result<Self, String> {
        let saved = stty(&["-g"])?;
        // Keys arrive one by one, unechoed; Ctrl-C arrives as a key, so the
        // terminal is always put back.
        stty(&["-icanon", "-echo", "-isig", "min", "0", "time", "1"])?;
        let terminal = Self { saved };
        put("\x1b[?1049h\x1b[?25l")?;
        Ok(terminal)
    }
}

fn put(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}

/// Clears the screen and draws `lines`, each cut to the terminal's width
/// and as many as fit its height.
fn draw(lines: &[String]) -> Result<(), String> {
    let size = stty(&["size"]).unwrap_or_default();
    // A terminal that does not know its size says 0 0.
    let mut dims = size
        .split_whitespace()
        .map(|n| n.parse::<usize>().ok().filter(|&n| n > 0));
    let mut next = || dims.next().flatten().unwrap_or(usize::MAX);
    let (rows, cols) = (next(), next());
    let mut screen = String::from("\x1b[H\x1b[2J");
    for (i, l) in lines.iter().take(rows).enumerate() {
        if i > 0 {
            screen.push_str("\r\n");
        }
        let _ = write!(screen, "{}", l.chars().take(cols).collect::<String>());
    }
    put(&screen)
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = put("\x1b[?25h\x1b[?1049l");
        let _ = stty(&[&self.saved]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_read_letters_arrows_and_ctrl_c() {
        assert_eq!(
            keys(b"jk\x1b[A\x1b[BrsSlq\x03x"),
            [
                Key::Down,
                Key::Up,
                Key::Up,
                Key::Down,
                Key::Order("restart"),
                Key::Order("stop"),
                Key::Order("start"),
                Key::Logs,
                Key::Quit,
                Key::Quit,
                Key::Other,
            ]
        );
        assert_eq!(keys(b"\x1b"), [Key::Other]);
    }

    #[test]
    fn render_marks_the_selection_and_keeps_going_past_errors() {
        let frame = json!({
            "servers": [
                {"name": "a", "addr": "127.0.0.1:1", "state": "ready", "pid": 7, "restarts": 0},
                {"name": "b\u{1b}[2J", "addr": "127.0.0.1:2", "state": "stopped", "restarts": 1},
            ],
            "machine": {"error": "no memory reading"},
            "journal": [{"kind": "note", "taken_at_ms": 0, "summary": "hi\u{1b}]0;x\u{7}"}],
        });
        let view = View {
            selected: 1,
            logs: Some(("b".into(), vec!["out\u{1b}[31m".into()])),
            message: "restart of b requested".into(),
        };
        let lines = render(&frame, &view, true);
        let text = lines.join("\n");
        assert!(lines.iter().any(|l| l.starts_with("  a ")), "{text}");
        assert!(lines.iter().any(|l| l.starts_with("> b[2J")), "{text}");
        assert!(text.contains("no memory reading"), "{text}");
        assert!(text.contains("output of b"), "{text}");
        assert!(text.contains("restart of b requested"), "{text}");
        assert!(
            !text.contains('\u{1b}') && !text.contains('\u{7}'),
            "{text}"
        );
        let plain = render(&frame, &View::default(), false).join("\n");
        assert!(!plain.contains('>'), "{plain}");
    }

    #[test]
    fn render_says_when_hearthd_is_down() {
        let frame = json!({
            "servers": {"error": "hearthd is not running"},
            "machine": {"health": {}, "findings": []},
            "journal": [],
        });
        let text = render(&frame, &View::default(), false).join("\n");
        assert!(text.contains("hearthd is not running"), "{text}");
    }
}
