//! One load at a time (`supervise.serialize_loads`): a server about to
//! start waits while another owned server is still loading, so two models
//! never fill memory at the same moment. This is ADR-000's fallback while
//! its measurement gate cannot run: measure-free, and safe.

use std::sync::{Mutex, PoisonError};

use hearth_runtime::supervise::{Event, Supervised};

/// The server loading now, if any.
static LOADING: Mutex<Option<String>> = Mutex::new(None);

/// Takes the load turn for `name`; `Err` names the server holding it.
pub fn take(name: &str) -> Result<(), String> {
    let mut loading = LOADING.lock().unwrap_or_else(PoisonError::into_inner);
    match loading.as_deref() {
        Some(other) if other != name => Err(other.to_string()),
        _ => {
            *loading = Some(name.to_string());
            Ok(())
        }
    }
}

/// Gives the turn back, if `name` holds it.
pub fn release(name: &str) {
    let mut loading = LOADING.lock().unwrap_or_else(PoisonError::into_inner);
    if loading.as_deref() == Some(name) {
        *loading = None;
    }
}

/// The server's last footprint at ready, from the run journal.
fn learned(name: &str) -> Option<u64> {
    let config = hearth_core::config::load().ok()?;
    let journal = hearth_core::state::Journal::open(&config.state).ok()?;
    crate::measured::last_footprint(&journal, name)
}

/// Whether a server whose last start held `learned` bytes may start with
/// `available` free: it needs at least that plus `headroom`. A server never
/// measured starts (serialized loads still apply); memory that cannot be
/// read holds the start, since hearth never admits blind.
fn admit(
    learned: Option<u64>,
    available: Result<u64, String>,
    headroom: u64,
) -> Result<(), String> {
    let Some(need) = learned else { return Ok(()) };
    let available = available.map_err(|e| format!("memory cannot be read ({e})"))?;
    let want = need.saturating_add(headroom);
    if available >= want {
        Ok(())
    } else {
        Err(format!(
            "its last start held {}; {} available, {} wanted with headroom",
            crate::units::bytes(need),
            crate::units::bytes(available),
            crate::units::bytes(want)
        ))
    }
}

/// One server's place in the load order. Gives the turn back when dropped,
/// also on a panic.
pub struct Gate<'a> {
    name: &'a str,
    serialize: bool,
    /// Headroom kept above a server's last footprint before it may start;
    /// `None` when admission by footprint is off.
    headroom: Option<u64>,
    /// Its last footprint at ready, once read for this wait.
    learned: Option<u64>,
    /// Whether `learned` has been read since the server last became ready.
    read: bool,
    /// Why its start is held, while it is.
    pub held: Option<String>,
    /// The server this one last waited for, so the wait is logged once.
    deferred_by: Option<String>,
}

impl<'a> Gate<'a> {
    pub fn new(name: &'a str, supervise: &hearth_core::config::Supervise) -> Self {
        Self {
            name,
            serialize: supervise.serialize_loads,
            headroom: supervise
                .admit_by_footprint
                .then_some(supervise.admit_headroom_bytes),
            learned: None,
            read: false,
            held: None,
            deferred_by: None,
        }
    }

    /// Polls the server; a poll that would start it first takes the load
    /// turn, and is put off while another server is loading.
    pub fn poll(&mut self, supervised: &mut Supervised) -> Option<Event> {
        let waiting = supervised.status().0 == "waiting";
        if waiting && let Some(headroom) = self.headroom {
            if !self.read {
                self.learned = learned(self.name);
                self.read = true;
            }
            let available = crate::pressure::host_memory().map(|m| m.available_bytes);
            match admit(self.learned, available, headroom) {
                Ok(()) => self.held = None,
                Err(why) => {
                    if self.held.as_deref() != Some(why.as_str()) && self.held.is_none() {
                        super::log(&format!("[{}] start held: {why}", self.name));
                    }
                    self.held = Some(why);
                    return None;
                }
            }
        }
        if self.serialize && waiting {
            match take(self.name) {
                Ok(()) => self.deferred_by = None,
                Err(other) => {
                    if self.deferred_by.as_deref() != Some(&other) {
                        super::log(&format!(
                            "[{}] waits to start: {other} is still loading",
                            self.name
                        ));
                    }
                    self.deferred_by = Some(other);
                    return None;
                }
            }
        }
        supervised.poll()
    }

    /// Gives the turn back once the server is no longer starting.
    pub fn settle(&mut self, state: &str) {
        if state != "starting" {
            release(self.name);
        }
        if state == "ready" {
            // A new footprint was just recorded; read it before the next start.
            self.read = false;
        }
    }
}

impl Drop for Gate<'_> {
    fn drop(&mut self) {
        release(self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_is_held_until_its_last_footprint_fits() {
        assert!(
            admit(None, Err("x".into()), 10).is_ok(),
            "never measured: starts"
        );
        assert!(admit(Some(100), Ok(110), 10).is_ok());
        let held = admit(Some(100), Ok(109), 10).unwrap_err();
        assert!(held.contains("available"), "{held}");
        assert!(
            admit(Some(1), Err("no".into()), 0)
                .unwrap_err()
                .contains("cannot be read")
        );
    }

    #[test]
    fn one_server_loads_at_a_time() {
        take("loads-a").unwrap();
        assert!(take("loads-a").is_ok(), "the holder may take it again");
        assert_eq!(take("loads-b"), Err("loads-a".into()));
        release("loads-b");
        assert_eq!(
            take("loads-b"),
            Err("loads-a".into()),
            "only the holder releases"
        );
        release("loads-a");
        assert!(take("loads-b").is_ok());
        release("loads-b");
    }
}
