//! Keeping a model server that hearth started alive, and only that one.
//!
//! Ported from Aletheia's supervisor (ADR-240 there), with hearth's rules:
//!
//! * **A port is not a model.** Before every start, the port is asked who
//!   answers; only a free port is started on. A started server is ready only
//!   when the port answers as a runtime serving the expected model and
//!   reports no loading state.
//! * **Bounded restarts.** A server that exits starts again after a back-off
//!   that grows by `backoff_factor` from `backoff_start` to `backoff_max`.
//!   More than `budget` exits inside `window` is a crash loop: the supervisor
//!   gives up and says so. A server that stayed ready a whole window earns the
//!   shortest back-off back.
//! * **Silence is a failure.** A ready server whose port stops answering as
//!   the expected model for `unresponsive_probes` probes in a row is stopped
//!   and counted as an exit: that is what a wedged scheduler, or a listener
//!   that died inside a live process, looks like from outside.
//! * **A poisoned server is restarted.** The server's output is read line by
//!   line (and passed on to hearth's stderr). A line that says the backend is
//!   in a permanent error state, like llama.cpp's Metal "recreate the backend
//!   to recover", stops and restarts it: such a server still answers
//!   `/health`, and fails every request until it is restarted.
//! * **A server never outlives its supervisor.** On Unix the server runs in
//!   its own process group, under a small `sh` tether that holds the
//!   supervisor's end of a pipe. When the pipe closes, because the
//!   supervisor stopped it, exited, or was killed with SIGKILL, the tether
//!   sends SIGTERM to the whole group, then SIGKILL after `stop_grace`. The
//!   group covers wrapper scripts and whatever they started. The escalation
//!   matters: a llama.cpp server whose Metal backend latched after an
//!   out-of-memory error does not exit on SIGTERM.
//!
//! The restart decision is [`Restarts`], a value with an injected clock, so
//! every rule is tested without a model. Every number comes from the caller,
//! which reads them from configuration.

use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::net::SocketAddr;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::fingerprint::{Identity, fingerprint};
use crate::llama_log::compute_bytes;
use crate::signals::{Cause, Signal, cause, classify};

mod tether;

use tether::ended;
pub use tether::tethered;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Exits tolerated inside one window before giving up.
    pub budget: u32,
    pub window: Duration,
    pub backoff_start: Duration,
    pub backoff_max: Duration,
    /// How much longer each back-off is than the one before.
    pub backoff_factor: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Start it again after this long.
    Restart { after_ms: u64 },
    /// It exited `exits` times inside one window: a crash loop.
    GiveUp { exits: usize, window_ms: u64 },
    /// Its log said why it exited, and a restart cannot cure that (a
    /// missing or unloadable model, a refused argument): it is not
    /// restarted until it is started again by hand.
    Futile,
}

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The restart ledger: exit times inside the window and the next back-off.
#[derive(Debug, Clone)]
pub struct Restarts {
    policy: RestartPolicy,
    exits: Vec<Duration>,
    backoff: Duration,
    decided: u64,
}

impl Restarts {
    #[must_use]
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            exits: Vec::new(),
            backoff: policy.backoff_start,
            decided: 0,
        }
    }

    /// Forgets the exits so far and the grown back-off: a person asking for
    /// a start is a new beginning, not one more crash.
    pub fn reset(&mut self) {
        self.exits.clear();
        self.backoff = self.policy.backoff_start;
    }

    /// Restarts decided so far.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.decided
    }

    /// A server exited at `now`, ready since `ready_since` (`None`: it never
    /// became ready). Both are offsets from the same epoch.
    pub fn on_exit(&mut self, now: Duration, ready_since: Option<Duration>) -> Decision {
        let window = self.policy.window;
        self.exits.retain(|&t| now.saturating_sub(t) < window);
        self.exits.push(now);
        if self.exits.len() > usize::try_from(self.policy.budget).unwrap_or(usize::MAX) {
            return Decision::GiveUp {
                exits: self.exits.len(),
                window_ms: ms(window),
            };
        }
        if ready_since.is_some_and(|t| now.saturating_sub(t) >= window) {
            self.backoff = self.policy.backoff_start;
        }
        let after = self.backoff;
        self.backoff = self
            .backoff
            .saturating_mul(self.policy.backoff_factor)
            .min(self.policy.backoff_max);
        self.decided += 1;
        Decision::Restart {
            after_ms: ms(after),
        }
    }
}

/// How the port is probed: the same bounded fingerprint `hearth ports` uses.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    pub addr: SocketAddr,
    pub deadline: Duration,
    pub max_bytes: u64,
}

impl Probe {
    fn identity(&self) -> Identity {
        fingerprint(self.addr, self.deadline, self.max_bytes).identity
    }
}

/// Whether `identity` is the expected model being served. With no expected
/// model, any runtime counts.
fn serves(identity: &Identity, expect: Option<&str>) -> bool {
    identity.is_runtime()
        && expect.is_none_or(|want| identity.models().iter().any(|m| m.contains(want)))
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "preflight", rename_all = "snake_case")]
pub enum Preflight {
    /// Nothing listens: start the server.
    Free,
    /// The expected model already answers: start nothing.
    AlreadyServing { identity: Identity },
    /// Something else answers: refuse, naming it.
    Occupied { identity: Identity },
}

/// Asks the port who is there before anything is started.
#[must_use]
pub fn preflight(probe: &Probe, expect: Option<&str>) -> Preflight {
    match probe.identity() {
        Identity::Refused => Preflight::Free,
        identity if serves(&identity, expect) => Preflight::AlreadyServing { identity },
        identity => Preflight::Occupied { identity },
    }
}

/// Everything a supervisor needs that is not the command.
#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub restart: RestartPolicy,
    /// How long a started server has to become ready.
    pub ready_timeout: Duration,
    /// SIGTERM to SIGKILL.
    pub stop_grace: Duration,
    /// Consecutive probes of a ready server that find it not serving before
    /// it is stopped.
    pub unresponsive_probes: u32,
    /// Lines of the server's output kept for [`Supervised::recent_output`].
    pub output_lines: usize,
    /// Longest line kept; the rest of a longer line is cut.
    pub output_line_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Started {
        pid: u32,
    },
    Ready {
        pid: u32,
        after_ms: u64,
        identity: Identity,
        /// The compute buffers llama.cpp logged while it loaded, summed
        /// over its backends, for `hearth fit` to learn from. Lines still
        /// unread when the port answered are not counted.
        #[serde(skip_serializing_if = "Option::is_none")]
        compute_bytes: Option<u64>,
    },
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
        /// Why, as the server's own log said, with the line that said it.
        #[serde(skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cause_line: Option<String>,
        #[serde(flatten)]
        decision: Decision,
    },
    /// Never became ready within the ready timeout; it was stopped.
    NotReady {
        after_ms: u64,
        /// What the port last answered.
        last: Identity,
        stop_error: Option<String>,
        #[serde(flatten)]
        decision: Decision,
    },
    /// Was ready, then stopped answering as the expected model; it was
    /// stopped.
    Wedged {
        misses: u32,
        last: Identity,
        stop_error: Option<String>,
        #[serde(flatten)]
        decision: Decision,
    },
    /// Something else holds the port, so nothing was started this time.
    PortTaken {
        identity: Identity,
        #[serde(flatten)]
        decision: Decision,
    },
    SpawnFailed {
        error: String,
        #[serde(flatten)]
        decision: Decision,
    },
    /// The server's own output said its backend is in a permanent error
    /// state; it was stopped.
    Poisoned {
        line: String,
        stop_error: Option<String>,
        #[serde(flatten)]
        decision: Decision,
    },
    /// The server's output reported something worth knowing that needs no
    /// action by itself, such as an accelerator out-of-memory error.
    Noticed {
        signal: Signal,
        line: String,
    },
    /// Stopped because someone asked for a restart; it starts again at once
    /// and the restart budget is not charged.
    Requested {
        stop_error: Option<String>,
    },
    /// Stopped because someone asked; it stays down until started again.
    Halted {
        stop_error: Option<String>,
    },
    /// The operating system could not say whether the server still runs; it
    /// was stopped.
    Lost {
        error: String,
        stop_error: Option<String>,
        #[serde(flatten)]
        decision: Decision,
    },
}

#[derive(Debug)]
enum State {
    Starting {
        child: Child,
        since: Instant,
    },
    Ready {
        child: Child,
        since: Instant,
        misses: u32,
    },
    Waiting {
        until: Instant,
    },
    GaveUp,
}

/// One server hearth owns. `make` builds its command afresh for each start.
pub struct Supervised {
    probe: Probe,
    expect: Option<String>,
    settings: Settings,
    make: Box<dyn Fn() -> Result<Command, String>>,
    restarts: Restarts,
    epoch: Instant,
    state: State,
    /// Signals read from the running server's output.
    signals: Option<Receiver<(Signal, String)>>,
    /// The last lines the server (and the servers before it) printed.
    output: Output,
    /// What the running server's log said since it started.
    log: StartLog,
}

/// What one start's output said, shared with the threads reading it.
#[derive(Default)]
struct Said {
    /// Compute buffers llama.cpp logged, summed over its backends.
    compute_bytes: u64,
    /// Lines read so far.
    lines: usize,
    /// The first line of the latest stretch that says why it would stop,
    /// with its line number: a cause is stale once more than the kept
    /// output (`supervise.output_lines`) has followed it.
    cause: Option<(Cause, String, usize)>,
    /// Output streams not yet read to their end.
    open: usize,
}

type StartLog = std::sync::Arc<(std::sync::Mutex<Said>, std::sync::Condvar)>;

fn said(log: &StartLog) -> std::sync::MutexGuard<'_, Said> {
    log.0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The most recent lines of output, shared with the threads reading it.
pub type Output = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<String>>>;

impl Supervised {
    #[must_use]
    pub fn new(
        probe: Probe,
        expect: Option<String>,
        settings: Settings,
        make: Box<dyn Fn() -> Result<Command, String>>,
    ) -> Self {
        let now = Instant::now();
        Self {
            probe,
            expect,
            settings,
            make,
            restarts: Restarts::new(settings.restart),
            epoch: now,
            state: State::Waiting { until: now },
            signals: None,
            output: Output::default(),
            log: StartLog::default(),
        }
    }

    #[must_use]
    pub fn gave_up(&self) -> bool {
        matches!(self.state, State::GaveUp)
    }

    #[must_use]
    pub fn ready(&self) -> bool {
        matches!(self.state, State::Ready { .. })
    }

    #[must_use]
    pub fn restarts(&self) -> u64 {
        self.restarts.count()
    }

    /// Records an exit at the current time, after any stop has finished, so
    /// the back-off is measured from when the server was really gone.
    fn after_exit(&mut self, ready_since: Option<Instant>) -> Decision {
        // What the old server said must not act on the next one.
        self.signals = None;
        let now = Instant::now();
        let at = |t: Instant| t.duration_since(self.epoch);
        let decision = self.restarts.on_exit(at(now), ready_since.map(at));
        self.state = match decision {
            Decision::Restart { after_ms } => State::Waiting {
                until: now + Duration::from_millis(after_ms),
            },
            Decision::GiveUp { .. } | Decision::Futile => State::GaveUp,
        };
        decision
    }

    /// Advances the server's life by one observation. Blocks for at most one
    /// fingerprint of the port (a few requests, each bounded by the probe
    /// deadline), plus the stop grace when a server has to be stopped.
    pub fn poll(&mut self) -> Option<Event> {
        if let Some((signal, line)) = self.signals.as_ref().and_then(|rx| rx.try_recv().ok()) {
            return Some(self.on_signal(signal, line));
        }
        match &mut self.state {
            State::GaveUp => None,
            State::Waiting { until } => {
                if Instant::now() < *until {
                    return None;
                }
                Some(self.start())
            }
            State::Starting { child, since } => {
                let since = *since;
                match child.try_wait() {
                    Ok(Some(status)) => return Some(self.exited(status, None)),
                    Ok(None) => {}
                    Err(e) => return Some(self.lost(&e, None)),
                }
                let identity = self.probe.identity();
                let now = Instant::now();
                if serves(&identity, self.expect.as_deref()) && identity.not_ready().is_none() {
                    let pid = child.id();
                    let State::Starting { child, .. } =
                        std::mem::replace(&mut self.state, State::GaveUp)
                    else {
                        return None;
                    };
                    self.state = State::Ready {
                        child,
                        since: now,
                        misses: 0,
                    };
                    let compute = said(&self.log).compute_bytes;
                    return Some(Event::Ready {
                        pid,
                        after_ms: ms(now.duration_since(since)),
                        identity,
                        compute_bytes: (compute > 0).then_some(compute),
                    });
                }
                let waited = now.duration_since(since);
                if waited < self.settings.ready_timeout {
                    return None;
                }
                let stop_error = stop(child).err();
                let decision = self.after_exit(None);
                Some(Event::NotReady {
                    after_ms: ms(waited),
                    last: identity,
                    stop_error,
                    decision,
                })
            }
            State::Ready {
                child,
                since,
                misses,
            } => {
                let since = *since;
                match child.try_wait() {
                    Ok(Some(status)) => return Some(self.exited(status, Some(since))),
                    Ok(None) => {}
                    Err(e) => return Some(self.lost(&e, Some(since))),
                }
                let identity = self.probe.identity();
                if serves(&identity, self.expect.as_deref()) {
                    *misses = 0;
                    return None;
                }
                *misses += 1;
                let misses = *misses;
                if misses < self.settings.unresponsive_probes {
                    return None;
                }
                let stop_error = stop(child).err();
                let decision = self.after_exit(Some(since));
                Some(Event::Wedged {
                    misses,
                    last: identity,
                    stop_error,
                    decision,
                })
            }
        }
    }

    /// Starts the server if its port is free.
    fn start(&mut self) -> Event {
        if let Preflight::AlreadyServing { identity } | Preflight::Occupied { identity } =
            preflight(&self.probe, self.expect.as_deref())
        {
            let decision = self.after_exit(None);
            return Event::PortTaken { identity, decision };
        }
        let spawned = (self.make)().and_then(|c| {
            tethered(&c, self.settings.stop_grace)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())
        });
        match spawned {
            Ok(mut child) => {
                let pid = child.id();
                let (tx, rx) = channel();
                // Each start's log is read on its own.
                self.log = StartLog::default();
                said(&self.log).open = [child.stdout.is_some(), child.stderr.is_some()]
                    .iter()
                    .filter(|&&open| open)
                    .count();
                let keep = (
                    self.output.clone(),
                    self.settings.output_lines,
                    self.settings.output_line_bytes,
                    self.log.clone(),
                );
                if let Some(out) = child.stdout.take() {
                    watch(out, tx.clone(), keep.clone());
                }
                if let Some(err) = child.stderr.take() {
                    watch(err, tx, keep);
                }
                self.signals = Some(rx);
                self.state = State::Starting {
                    child,
                    since: Instant::now(),
                };
                Event::Started { pid }
            }
            Err(error) => {
                let decision = self.after_exit(None);
                Event::SpawnFailed { error, decision }
            }
        }
    }

    fn on_signal(&mut self, signal: Signal, line: String) -> Event {
        let (child, ready_since) = match &mut self.state {
            State::Starting { child, .. } => (child, None),
            State::Ready { child, since, .. } => (child, Some(*since)),
            // Read after the server was already handled.
            State::Waiting { .. } | State::GaveUp => return Event::Noticed { signal, line },
        };
        match signal {
            Signal::Poisoned => {
                let stop_error = stop(child).err();
                let decision = self.after_exit(ready_since);
                Event::Poisoned {
                    line,
                    stop_error,
                    decision,
                }
            }
            Signal::OutOfMemory => Event::Noticed { signal, line },
        }
    }

    fn exited(&mut self, status: ExitStatus, ready_since: Option<Instant>) -> Event {
        // Closing the pipe lets the tether's watcher clear anything the
        // server left behind in its process group.
        if let State::Starting { child, .. } | State::Ready { child, .. } = &mut self.state {
            drop(child.stdin.take());
        }
        let (code, signal) = ended(status);
        // Its last lines may still be in the pipe: wait for the readers to
        // reach its end, as long as a stop would wait for the server. The
        // restart's backoff starts after this wait.
        let (cause, cause_line) = {
            let guard = said(&self.log);
            let (guard, _) = self
                .log
                .1
                .wait_timeout_while(guard, self.settings.stop_grace, |s| s.open > 0)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keep = self.settings.output_lines;
            guard
                .cause
                .clone()
                .filter(|(_, _, at)| guard.lines - at <= keep)
                .map(|(c, line, _)| (c, line))
                .unzip()
        };
        // A cause no restart can cure is not charged to the restart budget.
        // Only a start that never served can have one: a server that ran
        // and then died is restarted whatever it logged on the way.
        let decision = if ready_since.is_none() && cause.is_some_and(Cause::is_permanent) {
            self.signals = None;
            self.state = State::GaveUp;
            Decision::Futile
        } else {
            self.after_exit(ready_since)
        };
        Event::Exited {
            code,
            signal,
            cause,
            cause_line,
            decision,
        }
    }

    fn lost(&mut self, error: &std::io::Error, ready_since: Option<Instant>) -> Event {
        let stop_error = match &mut self.state {
            State::Starting { child, .. } | State::Ready { child, .. } => stop(child).err(),
            State::Waiting { .. } | State::GaveUp => None,
        };
        let decision = self.after_exit(ready_since);
        Event::Lost {
            error: error.to_string(),
            stop_error,
            decision,
        }
    }

    /// Keeps the server's output in `output`, which the caller can read from
    /// another thread.
    #[must_use]
    pub fn sharing_output(mut self, output: Output) -> Self {
        self.output = output;
        self
    }

    /// Stops the server on request; it stays down until [`Self::restart`].
    pub fn halt(&mut self) -> Event {
        Event::Halted {
            stop_error: self.stop().err(),
        }
    }

    /// The last lines the server printed, oldest first, across restarts.
    #[must_use]
    pub fn recent_output(&self) -> Vec<String> {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Where the server is in its life, and its pid when one runs.
    #[must_use]
    pub fn status(&self) -> (&'static str, Option<u32>) {
        match &self.state {
            State::Starting { child, .. } => ("starting", Some(child.id())),
            State::Ready { child, .. } => ("ready", Some(child.id())),
            State::Waiting { .. } => ("waiting", None),
            State::GaveUp => ("gave_up", None),
        }
    }

    /// Stops the server if one runs and starts it again at the next poll.
    /// Works after a give-up too: a person asking is a new start.
    pub fn restart(&mut self) -> Event {
        let stop_error = match &mut self.state {
            State::Starting { child, .. } | State::Ready { child, .. } => stop(child).err(),
            State::Waiting { .. } | State::GaveUp => None,
        };
        self.signals = None;
        self.restarts.reset();
        self.state = State::Waiting {
            until: Instant::now(),
        };
        Event::Requested { stop_error }
    }

    /// Stops the server, if one runs, and supervises nothing more.
    pub fn stop(&mut self) -> Result<(), String> {
        let result = match &mut self.state {
            State::Starting { child, .. } | State::Ready { child, .. } => stop(child),
            State::Waiting { .. } | State::GaveUp => Ok(()),
        };
        self.state = State::GaveUp;
        result
    }
}

/// Passes the server's output on to hearth's stderr line by line, and sends
/// every line that carries a signal. Ends when the server's group closes the
/// stream.
fn watch<R: Read + Send + 'static>(
    stream: R,
    signals: Sender<(Signal, String)>,
    (output, keep, line_bytes, log): (Output, usize, usize, StartLog),
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
            let _ = std::io::stderr().write_all(&line);
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

/// Closes the tether's pipe, which stops the server's process group with
/// SIGTERM and then SIGKILL after the grace, and waits for the tether to
/// report it gone. A process stuck in the kernel (a hung GPU driver) can
/// outlast SIGKILL; this then waits with it, since nothing else can be done.
fn stop(child: &mut Child) -> Result<(), String> {
    drop(child.stdin.take());
    #[cfg(not(unix))]
    child.kill().map_err(|e| e.to_string())?;
    child.wait().map(|_| ()).map_err(|e| e.to_string())
}

impl Drop for Supervised {
    fn drop(&mut self) {
        // Nothing is left to report to: the caller is gone.
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests;
