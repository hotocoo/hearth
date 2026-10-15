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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// The tether hearthd started; it stops the server when it goes.
    pid: Option<u32>,
    /// The process actually listening on the server's port: the server
    /// itself, under its tether and any wrapper script.
    server_pid: Option<i32>,
    /// That process's memory footprint, measured when status is asked.
    footprint_bytes: Option<u64>,
    /// Why the footprint could not be measured.
    footprint_error: Option<String>,
    restarts: u64,
    last_event: Option<Value>,
    last_event_at_ms: Option<u64>,
}

enum Order {
    Restart,
    Stop,
    Start,
    /// Stop the server and end its supervision: it left the configuration.
    Quit,
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
    /// Leaves `order` for the server; false when its supervision is
    /// ending, which nothing overrides.
    fn post(&self, order: Order) -> bool {
        let mut pending = self.order.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(*pending, Some(Order::Quit)) {
            return false;
        }
        *pending = Some(order);
        self.arrived.notify_one();
        true
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

/// A server hearthd keeps: its definition, its order mailbox, its recent
/// output, and the thread supervising it.
struct Kept {
    server: Server,
    mailbox: Arc<Mailbox>,
    output: hearth_runtime::supervise::Output,
    thread: std::thread::JoinHandle<()>,
}

/// What control requests reach: the servers kept now, which `reload`
/// changes, and the board they report to.
struct Desk {
    kept: Mutex<BTreeMap<String, Kept>>,
    /// A reload is being applied; orders wait until it is done.
    reloading: AtomicBool,
    board: Board,
    max_message_bytes: u64,
}

impl Desk {
    fn kept(&self) -> MutexGuard<'_, BTreeMap<String, Kept>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Starts supervising `server` on its own thread.
fn keep(name: &str, server: &Server, config: &Config, board: &Board) -> Result<Kept, String> {
    if let Some(t) = &server.cache_type {
        crate::fit::parse_cache(t).map_err(|e| format!("[{name}] cache_type: {e}"))?;
    }
    lock(board).insert(
        name.to_string(),
        Status {
            name: name.to_string(),
            addr: address(server),
            expect: server.expect.clone(),
            program: server.command.first().cloned().unwrap_or_default(),
            state: "waiting".into(),
            pid: None,
            server_pid: None,
            footprint_bytes: None,
            footprint_error: None,
            restarts: 0,
            last_event: None,
            last_event_at_ms: None,
        },
    );
    let mailbox = Arc::new(Mailbox::default());
    let output = hearth_runtime::supervise::Output::default();
    let thread = {
        let (name, server, config, board) = (
            name.to_string(),
            server.clone(),
            config.clone(),
            board.clone(),
        );
        let (mailbox, output) = (mailbox.clone(), output.clone());
        std::thread::Builder::new()
            .name(format!("supervise {name}"))
            .spawn(move || supervise(&name, &server, &config, &board, &mailbox, output))
    }
    .map_err(|e| {
        // Nothing supervises it: it must not look as if something did.
        lock(board).remove(name);
        format!("cannot start supervising {name}: {e}")
    })?;
    Ok(Kept {
        server: server.clone(),
        mailbox,
        output,
        thread,
    })
}

/// What `reload` will change: servers to add, to remove, and to replace.
fn plan_reload(
    kept: &BTreeMap<String, Server>,
    wanted: &BTreeMap<String, Server>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let added = wanted
        .keys()
        .filter(|n| !kept.contains_key(*n))
        .cloned()
        .collect();
    let removed = kept
        .keys()
        .filter(|n| !wanted.contains_key(*n))
        .cloned()
        .collect();
    let changed = wanted
        .iter()
        .filter(|(n, s)| kept.get(*n).is_some_and(|k| k != *s))
        .map(|(n, _)| n.clone())
        .collect();
    (added, removed, changed)
}

/// Re-reads the configuration and makes the kept servers match its
/// `[servers]`. Servers that left are stopped, changed ones are stopped and
/// started anew once the old one is gone (it holds the port until then),
/// new ones start, and the rest keep running untouched.
fn reload(desk: &Arc<Desk>) -> Value {
    // One reload at a time, from planning to the last server started.
    if desk
        .reloading
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return json!({ "error": "a reload is already in progress; try again when hearth status settles" });
    }
    let done = |answer: Value| {
        desk.reloading.store(false, Ordering::SeqCst);
        answer
    };
    let config = match hearth_core::config::load() {
        Ok(c) => c,
        Err(e) => {
            return done(
                json!({ "error": format!("not reloaded, the configuration does not load: {e}") }),
            );
        }
    };
    // Planned and taken out of service under one lock, so nothing else can
    // see a half-applied plan.
    let (added, removed, changed, leaving) = {
        let mut kept = desk.kept();
        let current: BTreeMap<String, Server> = kept
            .iter()
            .map(|(n, k)| (n.clone(), k.server.clone()))
            .collect();
        let (added, removed, changed) = plan_reload(&current, &config.servers);
        let leaving: Vec<(String, Kept)> = removed
            .iter()
            .chain(&changed)
            .filter_map(|n| kept.remove(n).map(|k| (n.clone(), k)))
            .collect();
        (added, removed, changed, leaving)
    };
    // Every leaving server is told at once, so their stops overlap.
    for (_, k) in &leaving {
        // False only when it was already leaving; either way it goes.
        let _ = k.mailbox.post(Order::Quit);
    }
    let answer = json!({ "added": added, "removed": removed, "changed": changed });
    let starting: Vec<String> = added.into_iter().chain(changed).collect();
    let worker = desk.clone();
    // Joining a stopping server can take its grace: the rest happens off
    // the control handler, and orders for these names are refused until it
    // is done (see `reloading`).
    let spawned = std::thread::Builder::new()
        .name("reload".into())
        .spawn(move || {
            for (name, k) in leaving {
                if k.thread.join().is_err() {
                    log(&format!("[{name}] its supervising thread had failed"));
                }
                lock(&worker.board).remove(&name);
                log(&format!("[{name}] left supervision"));
            }
            for name in starting {
                let Some(server) = config.servers.get(&name) else {
                    continue;
                };
                match keep(&name, server, &config, &worker.board) {
                    Ok(k) => {
                        worker.kept().insert(name.clone(), k);
                        log(&format!("[{name}] supervised"));
                    }
                    Err(e) => log(&e),
                }
            }
            worker.reloading.store(false, Ordering::SeqCst);
        });
    match spawned {
        Ok(_) => answer,
        Err(e) => done(json!({
            "error": format!("the reload could not run ({e}); the servers taken out were told to stop; run hearth reload again"),
        })),
    }
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
                s.server_pid = None;
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
    let (env, dir) = (server.env.clone(), server.dir.clone());
    // A relative model path is relative to where the server starts.
    let model_path = server.model.clone().map(|m| {
        let relative = std::path::Path::new(&m);
        match &server.dir {
            // Only a file that exists there: org/repo and Ollama names are
            // not paths.
            Some(d) if relative.is_relative() && d.join(relative).is_file() => {
                d.join(relative).display().to_string()
            }
            _ => m,
        }
    });
    // The projector the command loads counts too, where it can be found.
    let projector = crate::server_args::projector_arg(&command)
        .map(|p| match &server.dir {
            Some(d) if p.is_relative() => d.join(p),
            _ => p,
        })
        .filter(|p| p.is_file());
    let check = model_path
        .zip(server.ctx)
        .zip(server.cache_type.clone())
        .map(|((m, c), t)| {
            crate::fit::start_check(format!("[{}]", server.port), (m, c, t), projector)
        });
    let make = Box::new(move || {
        if let Some(check) = &check {
            check()?;
        }
        let (program, args) = command.split_first().ok_or("no command")?;
        // A relative path is relative to where the server starts.
        let path = std::path::Path::new(program);
        let check = match &dir {
            Some(d) if path.is_relative() && path.components().count() > 1 => d.join(path),
            _ => path.to_path_buf(),
        };
        crate::run::find_program(check.as_os_str())?;
        let mut c = Command::new(program);
        c.args(args).envs(&env);
        if let Some(d) = &dir {
            c.current_dir(d);
        }
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
    let journal = Journal::open(&config.state)
        .map_err(|e| log(&format!("[{name}] events will not be recorded: {e}")))
        .ok();
    let mut sending = Vec::new();
    let interval = Duration::from_millis(config.supervise.probe_interval_ms);
    loop {
        let mut events = Vec::new();
        match orders.take(interval).ok_or(()) {
            Ok(Order::Quit) => {
                if let Err(e) = supervised.stop() {
                    log(&format!("[{name}] stopping: {e}"));
                }
                return;
            }
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
        for event in events {
            use hearth_runtime::supervise::Event as E;
            // The server's pid is learned when it is ready and forgotten as
            // soon as it is gone, so nothing measures a pid since reused.
            let mut ready = None;
            let listening = match &event {
                E::Ready { pid, .. } => {
                    let found = crate::run::listener_of(address(server).port(), *pid);
                    if let Err(e) = &found {
                        log(&format!(
                            "cannot list processes to find the server's pid: {e}"
                        ));
                    }
                    let pid = found.clone().unwrap_or(None);
                    ready = Some(found);
                    Some(pid)
                }
                E::Started { .. } | E::Noticed { .. } => None,
                _ => Some(None),
            };
            if let Some(server_pid) = listening {
                update(board, name, |s| s.server_pid = server_pid);
            }
            let mut record = json!({
                "server": name,
                "addr": address(server),
                "expect": server.expect,
                "command": server.command.first(),
                "event": event,
            });
            if let Some(found) = &ready {
                crate::run::footprint_at_ready(&mut record, found);
            }
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
        // After the events, so status never shows a state whose details
        // (the server's pid, the last event) are not there yet.
        let (state, pid) = supervised.status();
        let state = if halted { "stopped" } else { state };
        let restarts = supervised.restarts();
        update(board, name, |s| {
            s.state = state.to_string();
            s.pid = pid;
            s.restarts = restarts;
        });
    }
}

fn answer(request: &str, board: &Board, desk: &Arc<Desk>) -> Value {
    let Ok(request) = serde_json::from_str::<Value>(request) else {
        return json!({ "error": "not a JSON request" });
    };
    match request["op"].as_str() {
        Some("status") => {
            let mut servers: Vec<Status> = lock(board).values().cloned().collect();
            for s in &mut servers {
                match s.server_pid.map(hearth_sense::process_footprint) {
                    Some(Ok(f)) => s.footprint_bytes = Some(f.footprint_bytes),
                    Some(Err(e)) => s.footprint_error = Some(e.to_string()),
                    None => {}
                }
            }
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
            let mailbox = desk.kept().get(name).map(|k| k.mailbox.clone());
            match mailbox {
                Some(_) if unsupervised => {
                    json!({ "error": format!("{name} is no longer supervised") })
                }
                Some(mailbox) if mailbox.post(order) => {
                    json!({ "requested": op, "server": name })
                }
                Some(_) => json!({ "error": format!("{name} is leaving supervision (reload)") }),
                None if desk.reloading.load(Ordering::SeqCst) => json!({
                    "error": format!("{name} is being reloaded; try again in a moment"),
                }),
                None => unknown(name),
            }
        }
        Some("logs") => {
            let name = request["server"].as_str().unwrap_or_default();
            let output = desk.kept().get(name).map(|k| k.output.clone());
            match output {
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
        Some("reload") => reload(desk),
        _ => json!({ "error": "op must be status, restart, stop, start, logs or reload" }),
    }
}

fn unknown(name: &str) -> Value {
    json!({ "error": format!("no server named {name:?}; hearth status lists them") })
}

/// Answers one client. The deadline bounds how long a stalled client can
/// hold its handler.
fn serve(stream: &UnixStream, config: &Config, board: &Board, desk: &Arc<Desk>) {
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
    if config.servers.is_empty() && !config.watch.in_hearthd {
        return Err(
            "nothing to do: no servers to keep and watch.in_hearthd is false; add [servers.NAME] with port and command (hearth config servers) or turn watching on"
                .into(),
        );
    }
    let (listener, _held) = bind()?;
    let board: Board = Arc::default();
    let mut kept = BTreeMap::new();
    for (name, server) in &config.servers {
        kept.insert(name.clone(), keep(name, server, &config, &board)?);
    }
    let count = kept.len();
    if config.watch.in_hearthd {
        let config = config.clone();
        std::thread::Builder::new()
            .name("watch".into())
            .spawn(move || crate::watch::forever(&config))
            .map_err(|e| format!("cannot start watching: {e}"))?;
    }
    let servers: crate::gateway::Servers = {
        let board = board.clone();
        Arc::new(move || {
            lock(&board)
                .values()
                .filter_map(|s| serde_json::to_value(s).ok())
                .collect()
        })
    };
    if let Some(addr) = config.metrics.listen {
        let listener =
            std::net::TcpListener::bind(addr).map_err(|e| format!("metrics.listen {addr}: {e}"))?;
        let (servers, control) = (servers.clone(), config.control.clone());
        std::thread::Builder::new()
            .name("metrics".into())
            .spawn(move || crate::metrics::serve(&listener, &control, &servers))
            .map_err(|e| format!("cannot start serving metrics: {e}"))?;
        log(&format!("hearthd: metrics on http://{addr}/metrics"));
    }
    if let Some(addr) = crate::gateway::start(&config.gateway, &servers)? {
        log(&format!("hearthd: gateway on http://{addr}/s/NAME/"));
    }
    if let Some(addr) = crate::dashboard::start(&config.dashboard)? {
        log(&format!("hearthd: dashboard on http://{addr}/"));
    }
    log(&format!(
        "hearthd: keeping {} server(s); control socket {}",
        count,
        crate::control::socket_path()?.display()
    ));
    let pause = Duration::from_millis(config.control.timeout_ms);
    let desk = Arc::new(Desk {
        kept: Mutex::new(kept),
        reloading: AtomicBool::new(false),
        board: board.clone(),
        max_message_bytes: config.control.max_message_bytes,
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

#[cfg(test)]
mod tests;
