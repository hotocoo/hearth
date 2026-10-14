//! `hearth watch`: keep observing the machine, record every observation,
//! and say so when something needs a person.
//!
//! Each round of `doctor` and `ports` is the same as running the command:
//! measured, recorded in the journal (so `log` and `diff` cover the time
//! watched), and compared with the rounds before. A problem is reported once
//! when it appears and once when it clears, on stdout and through
//! `[notify]`, never again on every round it lasts. A round that fails is a
//! problem like any other: the watch goes on, and stops only when nobody can
//! read its output any more.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Args {
    /// Stop after this many seconds; without it, watch until interrupted.
    #[arg(long, value_name = "SECONDS")]
    for_seconds: Option<u64>,
}

struct Entry {
    text: String,
    /// Rounds in a row it must be seen to open, and missed to clear.
    confirm: u32,
    seen: u32,
    missed: u32,
    reported: bool,
}

/// Problems seen so far, with how many rounds in a row each has been seen
/// or missed.
#[derive(Default)]
struct Tracker {
    entries: BTreeMap<String, Entry>,
}

/// `(key, text)` pairs.
type Problems = Vec<(String, String)>;
/// One round's problems as `(key, text, confirm rounds)`.
type Seen = Vec<(String, String, u32)>;

impl Tracker {
    /// Feeds one round's problems as `(key, text, confirm)`, and returns what
    /// is now to be reported opened and cleared. A problem opens once it has
    /// been seen `confirm` rounds in a row, and clears once it has been
    /// missed as many.
    fn round(&mut self, now: Seen) -> (Problems, Problems) {
        let mut opened = Vec::new();
        let mut present = std::collections::BTreeSet::new();
        for (key, text, confirm) in now {
            let e = self.entries.entry(key.clone()).or_insert(Entry {
                text: String::new(),
                confirm,
                seen: 0,
                missed: 0,
                reported: false,
            });
            e.text = text;
            e.confirm = confirm;
            e.seen += 1;
            e.missed = 0;
            if !e.reported && e.seen >= confirm {
                e.reported = true;
                opened.push((key.clone(), e.text.clone()));
            }
            present.insert(key);
        }
        let mut cleared = Vec::new();
        self.entries.retain(|key, e| {
            if present.contains(key) {
                return true;
            }
            e.seen = 0;
            e.missed += 1;
            if e.reported && e.missed >= e.confirm {
                cleared.push((key.clone(), e.text.clone()));
                return false;
            }
            // Unreported problems that went away were never news.
            e.reported
        });
        (opened, cleared)
    }
}

struct Out {
    json: bool,
    /// Whether a closed stdout ends the watch: yes for the command, whose
    /// reader is gone; no inside hearthd, whose watching matters more than
    /// its log.
    strict: bool,
    notify: hearth_core::config::Notify,
    sending: Vec<JoinHandle<()>>,
}

impl Out {
    fn say(&mut self, value: &Value, line: &str) -> Result<(), String> {
        let text = if self.json {
            value.to_string()
        } else {
            line.to_string()
        };
        let mut stdout = std::io::stdout().lock();
        let written = writeln!(stdout, "{text}").and_then(|()| stdout.flush());
        match written {
            Err(e) if self.strict => Err(format!("output closed: {e}")),
            _ => Ok(()),
        }
    }

    fn notify(&mut self, title: &str, text: &str, event: &Value) {
        self.sending.retain(|h| !h.is_finished());
        self.sending
            .extend(crate::notify::send(&self.notify, title, text, event));
    }

    /// Prints and notifies what opened and what cleared.
    fn report(
        &mut self,
        source: &str,
        (opened, cleared): (Problems, Problems),
    ) -> Result<(), String> {
        for (key, text) in opened {
            let event =
                json!({ "event": "problem", "source": source, "key": key, "message": text });
            self.say(&event, &format!("[{source}] {text}"))?;
            self.notify(&format!("hearth: {source}"), &text, &event);
        }
        for (key, text) in cleared {
            let event =
                json!({ "event": "cleared", "source": source, "key": key, "message": text });
            self.say(&event, &format!("[{source}] cleared: {text}"))?;
            self.notify(&format!("hearth: {source} cleared"), &text, &event);
        }
        Ok(())
    }
}

fn doctor_round(
    previous: Option<&crate::doctor::Observation>,
    horizon: Duration,
    confirm: u32,
) -> Result<(crate::doctor::Observation, Seen), String> {
    let observation = crate::doctor::observe(previous)?;
    let mut problems: Seen = observation
        .alarms(horizon)
        .into_iter()
        .map(|a| (a.key, a.text, if a.confirm { confirm } else { 1 }))
        .collect();
    problems.extend(
        observation
            .state_errors()
            .iter()
            .map(|e| (format!("history:{e}"), format!("history: {e}"), 1)),
    );
    Ok((observation, problems))
}

/// A runtime change is an event, not a state: each is reported once.
fn ports_round(
    out: &mut Out,
    previous: Option<&Value>,
    known: &mut std::collections::BTreeSet<(String, u16)>,
) -> Result<(Value, Seen), String> {
    let (report, _, errors) = crate::ports::observe_recorded()?;
    known.extend(crate::ports::runtime_keys(&report));
    if let Some(before) = previous {
        for c in crate::ports::alarms(before, &report, known) {
            let text = format!(
                "{} {}: {} → {}",
                c.subject,
                c.kind,
                c.before.as_deref().unwrap_or("-"),
                c.after.as_deref().unwrap_or("-")
            );
            let event =
                json!({ "event": "problem", "source": "ports", "change": c, "message": text });
            out.say(&event, &format!("[ports] {text}"))?;
            out.notify("hearth: runtime", &text, &event);
        }
    }
    // Warnings about how a server runs (configured past the device, slow,
    // silent) are states: reported once when they appear and when they clear.
    let mut problems: Seen = report["findings"]
        .as_array()
        .into_iter()
        .flatten()
        // A runtime going silent is already reported as a change.
        .filter(|f| f["severity"] == "warn" && f["code"] != "unresponsive")
        .map(|f| {
            (
                format!("{}:{}", f["code"].as_str().unwrap_or("finding"), f["pid"]),
                format!(
                    "{}; {}",
                    f["message"].as_str().unwrap_or_default(),
                    f["remediation"].as_str().unwrap_or_default()
                ),
                1,
            )
        })
        .collect();
    problems.extend(
        errors
            .iter()
            .map(|e| (format!("history:{e}"), format!("history: {e}"), 1)),
    );
    Ok((report, problems))
}

/// Watches until the process ends, for hearthd: the same rounds, records and
/// notifications as `hearth watch`, logged as plain lines.
pub fn forever(config: &hearth_core::config::Config) {
    let w = &config.watch;
    let mut out = Out {
        json: false,
        strict: false,
        notify: config.notify.clone(),
        sending: Vec::new(),
    };
    let every = (
        Duration::from_millis(w.doctor_interval_ms),
        Duration::from_millis(w.ports_interval_ms),
        Duration::from_millis(w.limit_horizon_ms),
    );
    // Only output can fail the loop, and output never fails here.
    let _ = watch(&mut out, w, None, every);
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let w = &config.watch;
    let doctor_every = Duration::from_millis(w.doctor_interval_ms);
    let ports_every = Duration::from_millis(w.ports_interval_ms);
    let horizon = Duration::from_millis(w.limit_horizon_ms);
    let until = match args.for_seconds {
        Some(s) => Some(
            Instant::now()
                .checked_add(Duration::from_secs(s))
                .ok_or("--for-seconds is too large for this platform's clock")?,
        ),
        None => None,
    };
    #[cfg(unix)]
    if let Some(why) = crate::daemon::watching() {
        return Err(why);
    }
    let mut out = Out {
        strict: true,
        json,
        notify: config.notify.clone(),
        sending: Vec::new(),
    };
    let result = watch(&mut out, w, until, (doctor_every, ports_every, horizon));
    for s in out.sending.drain(..) {
        // A delivery thread that panicked has already said so on stderr.
        let _ = s.join();
    }
    result
}

fn watch(
    out: &mut Out,
    w: &hearth_core::config::Watch,
    until: Option<Instant>,
    (doctor_every, ports_every, horizon): (Duration, Duration, Duration),
) -> Result<String, String> {
    out.say(
        &json!({ "event": "watching", "doctor_interval_ms": w.doctor_interval_ms, "ports_interval_ms": w.ports_interval_ms, "limit_horizon_ms": w.limit_horizon_ms, "confirm_rounds": w.confirm_rounds }),
        &format!(
            "watching: doctor every {doctor_every:?}, ports every {ports_every:?}; resources reaching a limit within {horizon:?} are reported"
        ),
    )?;
    let (mut doctor_tracker, mut ports_tracker) = (Tracker::default(), Tracker::default());
    let mut doctor: Option<crate::doctor::Observation> = None;
    let mut ports: Option<Value> = None;
    let mut known_runtimes = std::collections::BTreeSet::new();
    let (mut next_doctor, mut next_ports) = (Instant::now(), Instant::now());
    loop {
        let now = Instant::now();
        if until.is_some_and(|u| now >= u) {
            return Ok(String::new());
        }
        if now >= next_doctor {
            next_doctor = now + doctor_every;
            let problems = match doctor_round(doctor.as_ref(), horizon, w.confirm_rounds) {
                Ok((observation, problems)) => {
                    doctor = Some(observation);
                    problems
                }
                Err(e) => vec![("failed".into(), format!("doctor failed: {e}"), 1)],
            };
            out.report("doctor", doctor_tracker.round(problems))?;
        }
        if now >= next_ports {
            next_ports = now + ports_every;
            let problems = match ports_round(out, ports.as_ref(), &mut known_runtimes) {
                Ok((report, problems)) => {
                    ports = Some(report);
                    problems
                }
                Err(e) if e.starts_with("output closed") => return Err(e),
                Err(e) => vec![("failed".into(), format!("ports failed: {e}"), 1)],
            };
            out.report("ports", ports_tracker.round(problems))?;
        }
        let wake = next_doctor.min(next_ports);
        let wake = until.map_or(wake, |u| wake.min(u));
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Tracker, items: &[(&str, u32)]) -> (Vec<String>, Vec<String>) {
        let (opened, cleared) = t.round(
            items
                .iter()
                .map(|(k, c)| ((*k).to_string(), format!("{k} text"), *c))
                .collect(),
        );
        let keys = |v: Problems| v.into_iter().map(|(k, _)| k).collect();
        (keys(opened), keys(cleared))
    }

    #[test]
    fn a_problem_opens_once_and_clears_once() {
        let mut t = Tracker::default();
        assert_eq!(
            feed(&mut t, &[("memory_pressure:warn", 1)]).0,
            ["memory_pressure:warn"]
        );
        assert_eq!(
            feed(&mut t, &[("memory_pressure:warn", 1)]),
            (vec![], vec![])
        );
        // Warn rising to critical is a new problem; warn clears.
        assert_eq!(
            feed(&mut t, &[("memory_pressure:critical", 1)]),
            (
                vec!["memory_pressure:critical".into()],
                vec!["memory_pressure:warn".into()]
            )
        );
    }

    #[test]
    fn a_wobbling_projection_needs_its_confirm_rounds_both_ways() {
        let mut t = Tracker::default();
        // Seen once, gone, seen once: never confirmed, never reported.
        assert_eq!(feed(&mut t, &[("threads:7", 2)]), (vec![], vec![]));
        assert_eq!(feed(&mut t, &[]), (vec![], vec![]));
        assert_eq!(feed(&mut t, &[("threads:7", 2)]), (vec![], vec![]));
        assert_eq!(feed(&mut t, &[("threads:7", 2)]).0, ["threads:7"]);
        // One round missing does not clear it; two do.
        assert_eq!(feed(&mut t, &[]), (vec![], vec![]));
        assert_eq!(feed(&mut t, &[("threads:7", 2)]), (vec![], vec![]));
        assert_eq!(feed(&mut t, &[]), (vec![], vec![]));
        assert_eq!(feed(&mut t, &[]).1, ["threads:7"]);
    }
}
