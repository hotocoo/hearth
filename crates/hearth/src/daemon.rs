//! `hearthd`: keeps every server in `[servers]` alive, and answers `hearth
//! status` and `hearth restart` on the control socket.
//!
//! Each server is supervised exactly as `hearth run` supervises one: the same
//! preflight, restarts, wedge and poison detection, tether, journal records
//! (kind `run`, with the server's name) and notifications. hearthd only owns
//! servers; it proxies no inference.
//!
//! hearthd has no shutdown handler: however it ends, SIGKILL included, the
//! kernel closes each tether's pipe and the servers stop with it. A control
//! socket left behind is found stale and replaced at the next start.

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hearth_core::config::{Config, Server};
use hearth_core::state::Journal;
use hearth_runtime::supervise::{Probe, Supervised};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize)]
struct Status {
    name: String,
    addr: SocketAddr,
    expect: Option<String>,
    program: String,
    state: String,
    pid: Option<u32>,
    restarts: u64,
    last_event: Option<Value>,
    last_event_at_ms: Option<u64>,
}

enum Order {
    Restart,
}

type Board = Arc<Mutex<BTreeMap<String, Status>>>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn update(board: &Board, name: &str, change: impl FnOnce(&mut Status)) {
    if let Ok(mut b) = board.lock()
        && let Some(s) = b.get_mut(name)
    {
        change(s);
    }
}

/// Runs one server's life until hearthd ends. Orders arrive on `orders`;
/// waiting for them is the pause between polls, so a restart is immediate.
fn supervise(
    name: &str,
    server: &Server,
    config: &Config,
    board: &Board,
    orders: &Receiver<Order>,
) {
    let addr = SocketAddr::new(
        server.host.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        server.port,
    );
    let probe = Probe {
        addr,
        deadline: Duration::from_millis(config.discovery.first_contact_timeout_ms),
        max_bytes: config.discovery.max_probe_response_bytes,
    };
    let command = server.command.clone();
    let make = Box::new(move || {
        let (program, args) = command.split_first().ok_or("no command")?;
        crate::run::find_program(program.as_ref())?;
        let mut c = Command::new(program);
        c.args(args);
        Ok(c)
    });
    let mut supervised = Supervised::new(
        probe,
        server.expect.clone(),
        crate::run::settings(&config.supervise),
        make,
    );
    let journal = Journal::open(config.state.max_snapshots_per_kind)
        .map_err(|e| eprintln!("hearthd: {name}: events will not be recorded: {e}"))
        .ok();
    let mut sending = Vec::new();
    let interval = Duration::from_millis(config.supervise.probe_interval_ms);
    loop {
        let event = match orders.recv_timeout(interval) {
            Ok(Order::Restart) => Some(supervised.restart()),
            Err(RecvTimeoutError::Timeout) => supervised.poll(),
            // hearthd is shutting down this server's board.
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let (state, pid) = supervised.status();
        let restarts = supervised.restarts();
        update(board, name, |s| {
            s.state = state.to_string();
            s.pid = pid;
            s.restarts = restarts;
        });
        let Some(event) = event else { continue };
        let record = json!({
            "server": name,
            "addr": addr,
            "expect": server.expect,
            "command": server.command.first(),
            "event": event,
        });
        println!("[{name}] {}", crate::run::describe(&event));
        crate::run::keep(
            journal.as_ref(),
            &config.notify,
            &mut sending,
            (&format!("hearthd: {name}"), &event, &record),
        );
        let at = now_ms();
        update(board, name, |s| {
            s.last_event = serde_json::to_value(&event).ok();
            s.last_event_at_ms = Some(at);
        });
    }
}

fn answer(request: &str, board: &Board, orders: &BTreeMap<String, Sender<Order>>) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(request) else {
        return json!({ "error": "not a JSON request" });
    };
    match request["op"].as_str() {
        Some("status") => {
            let servers: Vec<Status> = board
                .lock()
                .map(|b| b.values().cloned().collect())
                .unwrap_or_default();
            json!({ "servers": servers })
        }
        Some("restart") => {
            let name = request["server"].as_str().unwrap_or_default();
            match orders.get(name) {
                Some(tx) if tx.send(Order::Restart).is_ok() => json!({ "requested": name }),
                Some(_) => json!({ "error": format!("{name} is no longer supervised") }),
                None => json!({
                    "error": format!("no server named {name:?}; hearth status lists them"),
                }),
            }
        }
        _ => json!({ "error": "op must be status or restart" }),
    }
}

fn serve(stream: &UnixStream, board: &Board, orders: &BTreeMap<String, Sender<Order>>) {
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let reply = answer(&line, board, orders);
    // A client that went away before the answer needs nothing more.
    let _ = writeln!(&*stream, "{reply}");
}

/// Binds the control socket. The state directory is made owner-only first,
/// since its permissions are what keep other users out. A socket nobody
/// answers on is left from an earlier hearthd and is replaced; one that
/// answers means hearthd already runs.
fn bind() -> Result<UnixListener, String> {
    let path = crate::control::socket_path()?;
    let dir = hearth_core::state::dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            return Err(format!(
                "hearthd already runs (it answers on {})",
                path.display()
            ));
        }
        std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    UnixListener::bind(&path).map_err(|e| crate::control::explain(&path, &e))
}

pub fn run() -> Result<(), String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    if config.servers.is_empty() {
        return Err(
            "no servers to keep: add [servers.NAME] with port and command to your config (hearth config servers)"
                .into(),
        );
    }
    let listener = bind()?;
    let board: Board = Arc::default();
    let mut orders = BTreeMap::new();
    for (name, server) in &config.servers {
        if let Ok(mut b) = board.lock() {
            b.insert(
                name.clone(),
                Status {
                    name: name.clone(),
                    addr: SocketAddr::new(
                        server.host.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                        server.port,
                    ),
                    expect: server.expect.clone(),
                    program: server.command.first().cloned().unwrap_or_default(),
                    state: "waiting".into(),
                    pid: None,
                    restarts: 0,
                    last_event: None,
                    last_event_at_ms: None,
                },
            );
        }
        let (tx, rx) = channel();
        orders.insert(name.clone(), tx);
        let (name, server, config, board) =
            (name.clone(), server.clone(), config.clone(), board.clone());
        std::thread::spawn(move || supervise(&name, &server, &config, &board, &rx));
    }
    println!(
        "hearthd: keeping {} server(s); control socket {}",
        orders.len(),
        crate::control::socket_path()?.display()
    );
    let orders = Arc::new(orders);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let (board, orders) = (board.clone(), orders.clone());
                std::thread::spawn(move || serve(&stream, &board, &orders));
            }
            Err(e) => eprintln!("hearthd: control socket: {e}"),
        }
    }
    Ok(())
}
