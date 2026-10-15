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
    /// Whether that answer reports a failure.
    failed: bool,
    /// The server a first `s` asked to stop; a second `s` stops it.
    armed: Option<String>,
    /// Colour and weight, on a terminal that has not asked for `NO_COLOR`.
    color: bool,
}

/// SGR codes, by what they mean on the screen.
const BOLD: &str = "1";
const DIM: &str = "2";
const OK: &str = "32";
const WAIT: &str = "33";
const BAD: &str = "31";
const ACCENT: &str = "36";

impl View {
    /// `text` in the style `code`, when colour is on.
    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

/// The colour of a server's state.
fn tone(state: &str) -> &'static str {
    match state {
        "ready" => OK,
        "starting" | "waiting" => WAIT,
        "gave_up" | "unsupervised" => BAD,
        _ => DIM,
    }
}

fn section(out: &mut Vec<String>, view: &View, title: &str) {
    out.push(String::new());
    out.push(format!(
        "{} {}",
        view.paint(DIM, "──"),
        view.paint(BOLD, title)
    ));
}

/// The frame as lines of text; `keys` adds the selection marker and help.
fn render(frame: &Value, view: &View, keys: bool) -> Vec<String> {
    let mut out = vec![if keys {
        format!(
            "{} {}",
            view.paint(&format!("{BOLD};{ACCENT}"), "hearth top"),
            view.paint(
                DIM,
                "· q quit · ↑↓ select · r restart · s s stop · S start · l logs"
            )
        )
    } else {
        "hearth top".to_string()
    }];
    section(&mut out, view, "servers (hearthd)");
    match frame["servers"].as_array() {
        Some(servers) if servers.is_empty() => {
            out.push("  none: add [servers.NAME] to your config".into());
        }
        Some(servers) => {
            for (i, s) in servers.iter().enumerate() {
                let selected = keys && i == view.selected;
                let mark = if selected { '>' } else { ' ' };
                let dot = if view.color {
                    format!(
                        "{} ",
                        view.paint(tone(s["state"].as_str().unwrap_or_default()), "●")
                    )
                } else {
                    String::new()
                };
                let text = clean(&line(s));
                let text = if selected {
                    view.paint(BOLD, &text)
                } else {
                    text
                };
                out.push(format!("{mark} {dot}{text}"));
            }
        }
        None => out.push(format!(
            "  {}",
            view.paint(BAD, &printable(&frame["servers"]["error"]))
        )),
    }
    section(&mut out, view, "machine");
    let machine = &frame["machine"];
    if machine["error"].is_string() {
        out.push(format!("  {}", printable(&machine["error"])));
    } else {
        out.push(format!("  {}", crate::doctor::summary(machine)));
        for f in machine["findings"].as_array().into_iter().flatten() {
            let severity = printable(&f["severity"]);
            let code = if severity == "critical" { BAD } else { WAIT };
            out.push(format!(
                "  {} {}",
                view.paint(code, &format!("[{severity}]")),
                printable(&f["message"])
            ));
        }
    }
    section(&mut out, view, "journal");
    match frame["journal"].as_array() {
        Some(entries) => {
            for e in entries {
                let kind = format!("{:<6}", printable(&e["kind"]));
                let age = format!(
                    "{:>10}",
                    crate::history::age(e["taken_at_ms"].as_u64().unwrap_or_default())
                );
                out.push(format!(
                    "  {} {}  {}",
                    view.paint(ACCENT, &kind),
                    view.paint(DIM, &age),
                    printable(&e["summary"])
                ));
            }
        }
        None => out.push(format!("  {}", printable(&frame["journal"]["error"]))),
    }
    if let Some((name, lines)) = &view.logs {
        section(
            &mut out,
            view,
            &format!("output of {} (l hides)", clean(name)),
        );
        out.extend(lines.iter().map(|l| format!("  {}", clean(l))));
    }
    if !view.message.is_empty() {
        out.push(String::new());
        let code = if view.failed { BAD } else { OK };
        out.push(view.paint(code, &clean(&view.message)));
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// What a key asks of the selected server, `name`. A first `s` only arms
/// the stop: stopping frees a model's memory, so one stray key must not.
fn press(view: &mut View, key: Key, name: &str) {
    let armed = view.armed.take();
    match key {
        Key::Order("stop") if armed.as_deref() != Some(name) => {
            view.armed = Some(name.to_string());
            view.failed = false;
            view.message =
                format!("press s again to stop {name}; it frees its memory until started");
        }
        Key::Order(op) => {
            (view.message, view.failed) = match call(&json!({ "op": op, "server": name })) {
                Ok(_) => (format!("{op} of {name} requested"), false),
                Err(e) => (format!("{op} of {name}: {e}"), true),
            };
        }
        Key::Logs => {
            view.logs = match view.logs.take() {
                Some((shown, _)) if shown == name => None,
                _ => Some((name.to_string(), Vec::new())),
            };
        }
        _ => {}
    }
}

fn interact(args: &Args) -> Result<(), String> {
    let mut view = View {
        color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        ..View::default()
    };
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
                    (Key::Up, _) => {
                        view.armed = None;
                        view.selected = view.selected.saturating_sub(1);
                    }
                    (Key::Down, _) => {
                        view.armed = None;
                        view.selected += 1;
                    }
                    (key, Some(name)) => press(&mut view, key, name),
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
        screen.push_str(&cut(l, cols));
    }
    put(&screen)
}

/// `line` cut to `cols` visible characters: escape sequences take no
/// room, and a cut line resets its style so colour cannot leak onward.
fn cut(line: &str, cols: usize) -> String {
    let mut out = String::new();
    let (mut shown, mut chars) = (0, line.chars());
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for e in chars.by_ref() {
                out.push(e);
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if shown < cols {
            out.push(c);
            shown += 1;
        }
    }
    if out.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
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
            ..View::default()
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
    fn colour_is_only_painted_when_asked_and_cut_by_what_shows() {
        let frame = json!({
            "servers": [{"name": "a", "addr": "127.0.0.1:1", "state": "gave_up", "restarts": 3}],
            "machine": {"health": {}, "findings": [{"severity": "critical", "message": "m"}]},
            "journal": [],
        });
        let painted = render(
            &frame,
            &View {
                color: true,
                ..View::default()
            },
            true,
        )
        .join("\n");
        assert!(painted.contains("\x1b[31m●"), "{painted}");
        assert!(painted.contains("\x1b[31m[critical]"), "{painted}");
        assert_eq!(cut("\x1b[1mabcdef\x1b[0m", 3), "\x1b[1mabc\x1b[0m\x1b[0m");
        assert_eq!(cut("abcdef", 3), "abc");
    }

    #[test]
    fn a_stop_needs_its_key_twice_and_anything_else_disarms_it() {
        let mut view = View::default();
        press(&mut view, Key::Order("stop"), "a");
        assert_eq!(view.armed.as_deref(), Some("a"));
        assert!(view.message.contains("press s again"), "{}", view.message);
        press(&mut view, Key::Logs, "a");
        assert_eq!(view.armed, None);
        assert!(view.logs.is_some());
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
