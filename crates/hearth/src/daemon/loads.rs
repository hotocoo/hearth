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

/// One server's place in the load order. Gives the turn back when dropped,
/// also on a panic.
pub struct Gate<'a> {
    name: &'a str,
    serialize: bool,
    /// The server this one last waited for, so the wait is logged once.
    deferred_by: Option<String>,
}

impl<'a> Gate<'a> {
    pub fn new(name: &'a str, serialize: bool) -> Self {
        Self {
            name,
            serialize,
            deferred_by: None,
        }
    }

    /// Polls the server; a poll that would start it first takes the load
    /// turn, and is put off while another server is loading.
    pub fn poll(&mut self, supervised: &mut Supervised) -> Option<Event> {
        if self.serialize && supervised.status().0 == "waiting" {
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
    pub fn settle(&self, state: &str) {
        if state != "starting" {
            release(self.name);
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
