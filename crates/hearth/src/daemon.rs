//! `hearthd`: keeps every server in `[servers]` alive, and answers `hearth
//! status` and `hearth restart` on the control socket.
//!
//! Each server is supervised exactly as `hearth run` supervises one: the same
//! preflight, restarts, wedge and poison detection, tether, journal records
//! (kind `run`, with the server's name) and notifications. hearthd only owns
//! servers; it proxies no inference.
//!
//! hearthd has no shutdown handler: however it ends, SIGKILL included, the
//! kernel closes each tether's pipe and the servers stop with it. One hearthd
//! per state directory is enforced by a lock the kernel drops when hearthd
//! ends, so a socket left behind is known to be stale and is replaced.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{FileTypeExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::sync::Condvar;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    Stop,
    Start,
}

/// One server's pending order. Only the latest counts: `stop` then `start`
/// before either is acted on means start, so no order is lost in a way that
/// leaves a server in the state nobody asked for last.
#[derive(Default)]
struct Mailbox {
    order: Mutex<Option<Order>>,
    arrived: Condvar,
}

impl Mailbox {
    fn post(&self, order: Order) {
        *self.order.lock().unwrap_or_else(PoisonError::into_inner) = Some(order);
        self.arrived.notify_one();
    }

    /// The pending order, waiting at most `timeout` for one.
    fn take(&self, timeout: Duration) -> Option<Order> {
        let guard = self.order.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut guard, _) = self
            .arrived
            .wait_timeout_while(guard, timeout, |o| o.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        guard.take()
    }
}

/// Each server's recent output, readable while its thread supervises it.
type Outputs = BTreeMap<String, hearth_runtime::supervise::Output>;

/// What control requests reach: each server's order queue and output.
struct Desk {
    orders: BTreeMap<String, Arc<Mailbox>>,
    max_message_bytes: u64,
    outputs: Outputs,
}

type Board = Arc<Mutex<BTreeMap<String, Status>>>;

/// The board holds plain values, so a thread that panicked while holding
/// it cannot have left it half-changed in a way that matters: recover.
fn lock(board: &Board) -> MutexGuard<'_, BTreeMap<String, Status>> {
    board.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn update(board: &Board, name: &str, change: impl FnOnce(&mut Status)) {
    if let Some(s) = lock(board).get_mut(name) {
        change(s);
    }
}

/// hearthd's own log line. A closed stdout loses the log, never the
/// supervision.
fn log(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

/// Marks a server's supervision as ended if its thread unwinds, so status
/// tells the truth instead of showing the last state forever.
struct Watchman<'a> {
    board: &'a Board,
    name: &'a str,
}

impl Drop for Watchman<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            update(self.board, self.name, |s| {
                s.state = "unsupervised".into();
                s.pid = None;
            });
        }
    }
}

fn supervised(server: &Server, config: &Config) -> Supervised {
    let probe = Probe {
        addr: address(server),
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
    Supervised::new(
        probe,
        server.expect.clone(),
        crate::run::settings(&config.supervise),
        make,
    )
}

fn address(server: &Server) -> SocketAddr {
    SocketAddr::new(
        server.host.unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        server.port,
    )
}

/// Runs one server's life until hearthd ends. Waiting for an order is the
/// pause between polls; a restart order is acted on at once.
fn supervise(
    name: &str,
    server: &Server,
    config: &Config,
    board: &Board,
    orders: &Mailbox,
    output: hearth_runtime::supervise::Output,
) {
    let _watchman = Watchman { board, name };
    let mut supervised = supervised(server, config).sharing_output(output);
    let mut halted = false;
    let journal = Journal::open(config.state.max_snapshots_per_kind)
        .map_err(|e| log(&format!("[{name}] events will not be recorded: {e}")))
        .ok();
    let mut sending = Vec::new();
    let interval = Duration::from_millis(config.supervise.probe_interval_ms);
    loop {
        let mut events = Vec::new();
        match orders.take(interval).ok_or(()) {
            Ok(Order::Restart) => {
                halted = false;
                events.push(supervised.restart());
                events.extend(supervised.poll());
            }
            Ok(Order::Start) if halted || supervised.gave_up() => {
                halted = false;
                events.push(supervised.restart());
                events.extend(supervised.poll());
            }
            Ok(Order::Stop) if !halted => {
                halted = true;
                events.push(supervised.halt());
            }
            // Starting what runs, or stopping what is stopped: nothing to do.
            Ok(Order::Start | Order::Stop) => {}
            Err(()) => events.extend(supervised.poll()),
        }
        let (state, pid) = supervised.status();
        let state = if halted { "stopped" } else { state };
        let restarts = supervised.restarts();
        update(board, name, |s| {
            s.state = state.to_string();
            s.pid = pid;
            s.restarts = restarts;
        });
        for event in events {
            let record = json!({
                "server": name,
                "addr": address(server),
                "expect": server.expect,
                "command": server.command.first(),
                "event": event,
            });
            log(&format!("[{name}] {}", crate::run::describe(&event)));
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
}

fn answer(request: &str, board: &Board, desk: &Desk) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(request) else {
        return json!({ "error": "not a JSON request" });
    };
    match request["op"].as_str() {
        Some("status") => {
            let servers: Vec<Status> = lock(board).values().cloned().collect();
            json!({ "servers": servers })
        }
        Some(op @ ("restart" | "stop" | "start")) => {
            let name = request["server"].as_str().unwrap_or_default();
            let order = match op {
                "stop" => Order::Stop,
                "start" => Order::Start,
                _ => Order::Restart,
            };
            let unsupervised = lock(board)
                .get(name)
                .is_some_and(|s| s.state == "unsupervised");
            match desk.orders.get(name) {
                Some(_) if unsupervised => {
                    json!({ "error": format!("{name} is no longer supervised") })
                }
                Some(mailbox) => {
                    mailbox.post(order);
                    json!({ "requested": op, "server": name })
                }
                None => unknown(name),
            }
        }
        Some("logs") => {
            let name = request["server"].as_str().unwrap_or_default();
            match desk.outputs.get(name) {
                Some(output) => {
                    let mut lines: Vec<String> = output
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .iter()
                        .cloned()
                        .collect();
                    // The answer must fit one control message: the newest
                    // lines are kept, and the answer says how many were not.
                    let limit = usize::try_from(desk.max_message_bytes).unwrap_or(usize::MAX);
                    let mut left_out = 0;
                    let mut reply = json!({ "server": name, "lines": lines, "left_out": 0 });
                    while reply.to_string().len() >= limit && !lines.is_empty() {
                        lines.remove(0);
                        left_out += 1;
                        reply = json!({ "server": name, "lines": lines, "left_out": left_out });
                    }
                    reply
                }
                None => unknown(name),
            }
        }
        _ => json!({ "error": "op must be status, restart, stop, start or logs" }),
    }
}

fn unknown(name: &str) -> Value {
    json!({ "error": format!("no server named {name:?}; hearth status lists them") })
}

/// Answers one client. The deadline bounds how long a stalled client can
/// hold its handler.
fn serve(stream: &UnixStream, config: &Config, board: &Board, desk: &Desk) {
    let deadline = Instant::now() + Duration::from_millis(config.control.timeout_ms);
    let reply = match crate::control::read_line(stream, config.control.max_message_bytes, deadline)
    {
        Ok(line) => answer(&line, board, desk),
        Err(e) => json!({ "error": format!("bad request: {e}") }),
    };
    if let Err(e) = crate::control::write_line(stream, &reply.to_string(), deadline) {
        log(&format!("control socket: could not answer a client: {e}"));
    }
}

/// A taken control handler slot, given back when dropped.
struct Slot<'a>(&'a AtomicU64);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Gives `stream` its own handler, up to `control.max_clients` at once; a
/// client beyond that is told hearthd is busy, so stalled clients can take
/// up handlers but never memory or threads without bound.
fn handle(
    stream: UnixStream,
    config: &Arc<Config>,
    board: &Board,
    desk: &Arc<Desk>,
    busy: &Arc<AtomicU64>,
) {
    let deadline = Instant::now() + Duration::from_millis(config.control.timeout_ms);
    let refuse = |why: &str| {
        let reply = json!({ "error": format!("hearthd is busy: {why}; try again") });
        if let Err(e) = crate::control::write_line(&stream, &reply.to_string(), deadline) {
            log(&format!("control socket: could not answer a client: {e}"));
        }
    };
    if busy.fetch_add(1, Ordering::SeqCst) >= config.control.max_clients {
        busy.fetch_sub(1, Ordering::SeqCst);
        refuse(&format!("{} clients at once", config.control.max_clients));
        return;
    }
    let (config, board, desk, done) = (config.clone(), board.clone(), desk.clone(), busy.clone());
    let spawned = std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            // Frees the slot however serve ends, a panic included.
            let _slot = Slot(&done);
            serve(&stream, &config, &board, &desk);
        });
    if let Err(e) = spawned {
        busy.fetch_sub(1, Ordering::SeqCst);
        log(&format!("control socket: no thread for a client: {e}"));
    }
}

/// Why `hearth watch` should not run: hearthd already watches this state
/// directory when it holds the lock and has watching on. The lock is only
/// tested, never kept.
#[must_use]
pub fn watching() -> Option<String> {
    let config = hearth_core::config::load().ok()?;
    if !config.watch.in_hearthd {
        return None;
    }
    let dir = hearth_core::state::dir().ok()?;
    let file = File::open(dir.join("hearthd.lock")).ok()?;
    match file.try_lock_shared() {
        Err(std::fs::TryLockError::WouldBlock) => Some(
            "hearthd already watches this machine (watch.in_hearthd); its findings are in its log and in hearth log --kind doctor / ports. Stop it or set watch.in_hearthd = false to watch here."
                .to_string(),
        ),
        _ => None,
    }
}

/// Holds the state directory's lock for as long as hearthd runs. The kernel
/// releases it however hearthd ends, so whoever holds it is the one live
/// hearthd, and a socket found without it is stale.
fn take_lock(dir: &std::path::Path) -> Result<File, String> {
    let path = dir.join("hearthd.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "hearthd already runs for {} (it holds {})",
            dir.display(),
            path.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

/// Binds the control socket in a private state directory, after taking the
/// lock. Only a socket is ever removed from the socket's path.
fn bind() -> Result<(UnixListener, File), String> {
    let dir = hearth_core::state::dir().map_err(|e| e.to_string())?;
    // Creates it owner-only when missing; never changes an existing one.
    hearth_core::state::ensure_dir(&dir).map_err(|e| e.to_string())?;
    crate::control::private(&dir, false)?;
    let held = take_lock(&dir)?;
    let path = crate::control::socket_path()?;
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_socket() => {
            std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "{} exists and is not a socket; move it away",
                path.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let listener = UnixListener::bind(&path).map_err(|e| crate::control::explain(&path, &e))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((listener, held))
}

pub fn run() -> Result<(), String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    if config.servers.is_empty() {
        return Err(
            "no servers to keep: add [servers.NAME] with port and command to your config (hearth config servers)"
                .into(),
        );
    }
    let (listener, _held) = bind()?;
    let board: Board = Arc::default();
    let mut orders = BTreeMap::new();
    let mut outputs = Outputs::new();
    for (name, server) in &config.servers {
        lock(&board).insert(
            name.clone(),
            Status {
                name: name.clone(),
                addr: address(server),
                expect: server.expect.clone(),
                program: server.command.first().cloned().unwrap_or_default(),
                state: "waiting".into(),
                pid: None,
                restarts: 0,
                last_event: None,
                last_event_at_ms: None,
            },
        );
        let mailbox = Arc::new(Mailbox::default());
        orders.insert(name.clone(), mailbox.clone());
        let output = hearth_runtime::supervise::Output::default();
        outputs.insert(name.clone(), output.clone());
        let (name, server, config, board) =
            (name.clone(), server.clone(), config.clone(), board.clone());
        std::thread::Builder::new()
            .name(format!("supervise {name}"))
            .spawn(move || supervise(&name, &server, &config, &board, &mailbox, output))
            .map_err(|e| format!("cannot start supervising: {e}"))?;
    }
    if config.watch.in_hearthd {
        let config = config.clone();
        std::thread::Builder::new()
            .name("watch".into())
            .spawn(move || crate::watch::forever(&config))
            .map_err(|e| format!("cannot start watching: {e}"))?;
    }
    if let Some(addr) = config.metrics.listen {
        let listener =
            std::net::TcpListener::bind(addr).map_err(|e| format!("metrics.listen {addr}: {e}"))?;
        let (board, control) = (board.clone(), config.control.clone());
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || {
                let servers = || -> Vec<Value> {
                    lock(&board)
                        .values()
                        .filter_map(|s| serde_json::to_value(s).ok())
                        .collect()
                };
                crate::metrics::serve(&listener, &control, &servers);
            })
            .map_err(|e| format!("cannot start serving metrics: {e}"))?;
        log(&format!("hearthd: metrics on http://{addr}/metrics"));
    }
    log(&format!(
        "hearthd: keeping {} server(s); control socket {}",
        orders.len(),
        crate::control::socket_path()?.display()
    ));
    let pause = Duration::from_millis(config.control.timeout_ms);
    let desk = Arc::new(Desk {
        orders,
        max_message_bytes: config.control.max_message_bytes,
        outputs,
    });
    let config = Arc::new(config);
    let busy = Arc::new(AtomicU64::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => handle(stream, &config, &board, &desk, &busy),
            Err(e) => {
                // Out of descriptors, typically: say so, and do not spin.
                log(&format!("control socket: {e}"));
                std::thread::sleep(pause);
            }
        }
    }
    Ok(())
}
