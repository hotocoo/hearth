//! `hearth watch`: keep observing the machine, record every observation,
//! and say so when something needs a person.
//!
//! Each round of `doctor` and `ports` is the same as running the command:
//! measured, recorded in the journal (so `log` and `diff` cover the time
//! watched), and compared with the round before. A problem is reported once
//! when it appears and once when it clears, on stdout and through
//! `[notify]`, never again on every round it lasts.

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

/// An open problem, keyed by what it is about.
type Open = BTreeMap<String, String>;

struct Out<'a> {
    stdout: std::io::StdoutLock<'a>,
    json: bool,
    notify: hearth_core::config::Notify,
    sending: Vec<JoinHandle<()>>,
}

impl Out<'_> {
    fn say(&mut self, value: &Value, line: &str) -> Result<(), String> {
        let text = if self.json {
            value.to_string()
        } else {
            line.to_string()
        };
        writeln!(self.stdout, "{text}")
            .and_then(|()| self.stdout.flush())
            .map_err(|e| format!("output closed: {e}"))
    }

    /// Reports what opened and what cleared between `was` and `now`.
    fn compare(&mut self, source: &str, was: &Open, now: &Open) -> Result<(), String> {
        let (opened, cleared) = transitions(was, now);
        for (key, text) in opened {
            {
                let event =
                    json!({ "event": "problem", "source": source, "key": key, "message": text });
                self.say(&event, &format!("[{source}] {text}"))?;
                self.sending.extend(crate::notify::send(
                    &self.notify,
                    &format!("hearth: {source}"),
                    text,
                    &event,
                ));
            }
        }
        for (key, text) in cleared {
            {
                let event =
                    json!({ "event": "cleared", "source": source, "key": key, "message": text });
                self.say(&event, &format!("[{source}] cleared: {text}"))?;
            }
        }
        Ok(())
    }

    fn errors(&mut self, source: &str, errors: &[String]) -> Result<(), String> {
        for e in errors {
            self.say(
                &json!({ "event": "history_error", "source": source, "message": e }),
                &format!("[{source}] history: {e}"),
            )?;
        }
        Ok(())
    }
}

type Entries<'a> = Vec<(&'a String, &'a String)>;

/// Problems in `now` that were not in `was`, and problems in `was` that are
/// no longer in `now`.
fn transitions<'a>(was: &'a Open, now: &'a Open) -> (Entries<'a>, Entries<'a>) {
    let opened = now.iter().filter(|(k, _)| !was.contains_key(*k)).collect();
    let cleared = was.iter().filter(|(k, _)| !now.contains_key(*k)).collect();
    (opened, cleared)
}

fn doctor_round(
    out: &mut Out,
    previous: Option<&crate::doctor::Observation>,
    open: &Open,
    horizon: Duration,
) -> Result<(crate::doctor::Observation, Open), String> {
    let observation = crate::doctor::observe(previous)?;
    let now: Open = observation
        .alarms(horizon)
        .into_iter()
        .map(|a| (a.key, a.text))
        .collect();
    out.errors("doctor", observation.state_errors())?;
    out.compare("doctor", open, &now)?;
    Ok((observation, now))
}

fn ports_round(out: &mut Out, previous: Option<&Value>) -> Result<Value, String> {
    let (report, _, errors) = crate::ports::observe_recorded()?;
    out.errors("ports", &errors)?;
    // A change is an event, not a state: it is reported once, when seen.
    if let Some(before) = previous {
        for c in crate::ports::alarms(before, &report) {
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
            out.sending.extend(crate::notify::send(
                &out.notify,
                "hearth: runtime",
                &text,
                &event,
            ));
        }
    }
    Ok(report)
}

pub fn run(args: &Args, json: bool) -> Result<String, String> {
    let config = hearth_core::config::load().map_err(|e| e.to_string())?;
    let w = &config.watch;
    let doctor_every = Duration::from_millis(w.doctor_interval_ms);
    let ports_every = Duration::from_millis(w.ports_interval_ms);
    let horizon = Duration::from_millis(w.limit_horizon_ms);
    let until = args
        .for_seconds
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut out = Out {
        stdout: std::io::stdout().lock(),
        json,
        notify: config.notify.clone(),
        sending: Vec::new(),
    };
    out.say(
        &json!({ "event": "watching", "doctor_interval_ms": w.doctor_interval_ms, "ports_interval_ms": w.ports_interval_ms, "limit_horizon_ms": w.limit_horizon_ms }),
        &format!(
            "watching: doctor every {doctor_every:?}, ports every {ports_every:?}; resources reaching a limit within {horizon:?} are reported"
        ),
    )?;
    let mut doctor: Option<crate::doctor::Observation> = None;
    let mut open = Open::new();
    let mut ports: Option<Value> = None;
    let (mut next_doctor, mut next_ports) = (Instant::now(), Instant::now());
    let result = loop {
        let now = Instant::now();
        if until.is_some_and(|u| now >= u) {
            break Ok(String::new());
        }
        if now >= next_doctor {
            next_doctor = now + doctor_every;
            match doctor_round(&mut out, doctor.as_ref(), &open, horizon) {
                Ok((observation, now_open)) => {
                    doctor = Some(observation);
                    open = now_open;
                }
                Err(e) => break Err(e),
            }
        }
        if now >= next_ports {
            next_ports = now + ports_every;
            match ports_round(&mut out, ports.as_ref()) {
                Ok(report) => ports = Some(report),
                Err(e) => break Err(e),
            }
        }
        let wake = next_doctor.min(next_ports);
        let wake = until.map_or(wake, |u| wake.min(u));
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
    };
    for s in out.sending.drain(..) {
        // A delivery thread that panicked has already said so on stderr.
        let _ = s.join();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(items: &[(&str, &str)]) -> Open {
        items
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn keys<'a>(v: &[(&'a String, &'a String)]) -> Vec<&'a str> {
        v.iter().map(|(k, _)| k.as_str()).collect()
    }

    #[test]
    fn a_problem_is_reported_when_it_opens_and_when_it_clears_only() {
        let was = open(&[("memory_pressure", "warn"), ("threads:7", "growing")]);
        // Same key, new wording (the rate moved): still the same problem.
        let now = open(&[
            ("memory_pressure", "warn, worse"),
            ("open_files:9", "growing"),
        ]);
        let (opened, cleared) = transitions(&was, &now);
        assert_eq!(keys(&opened), ["open_files:9"]);
        assert_eq!(keys(&cleared), ["threads:7"]);
        let (none, gone) = transitions(&now, &now);
        assert!(none.is_empty() && gone.is_empty());
    }
}
