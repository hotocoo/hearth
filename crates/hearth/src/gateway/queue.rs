//! Fair queuing in front of each server: at most `per_server_concurrency`
//! requests run on a server at once, and those waiting are let in round
//! robin across apps (the `X-Hearth-App` header), first come first served
//! within one app. One app's batch of a hundred requests then cannot hold
//! up another app's single request for the whole batch.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

/// Who is waiting for one server, and whose turn is next.
#[derive(Default)]
struct Line {
    running: u64,
    next_ticket: u64,
    /// Waiting tickets by app, oldest first.
    waiting: BTreeMap<String, VecDeque<u64>>,
    /// The app let in last; the next turn goes to the app after it.
    last: Option<String>,
    /// Tickets chosen to run that have not yet noticed.
    admitted: Vec<u64>,
}

impl Line {
    fn join(&mut self, app: &str) -> u64 {
        self.next_ticket += 1;
        self.waiting
            .entry(app.to_string())
            .or_default()
            .push_back(self.next_ticket);
        self.next_ticket
    }

    /// The app whose turn it is: the first waiting one after the last
    /// served, wrapping around.
    fn turn(&self) -> Option<String> {
        let after = self.last.as_deref().unwrap_or("");
        let ready = |(app, q): (&String, &VecDeque<u64>)| (!q.is_empty()).then(|| app.clone());
        self.waiting
            .range::<str, _>((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .find_map(ready)
            .or_else(|| self.waiting.iter().find_map(ready))
    }

    /// Lets waiting requests in while there is room.
    fn admit(&mut self, limit: u64) {
        while self.running < limit {
            let Some(app) = self.turn() else { return };
            let Some(ticket) = self.waiting.get_mut(&app).and_then(VecDeque::pop_front) else {
                return;
            };
            if self.waiting.get(&app).is_some_and(VecDeque::is_empty) {
                self.waiting.remove(&app);
            }
            self.last = Some(app);
            self.running += 1;
            self.admitted.push(ticket);
        }
    }

    /// Takes `ticket` out of line without running it (its deadline passed).
    fn leave(&mut self, ticket: u64) {
        for q in self.waiting.values_mut() {
            q.retain(|&t| t != ticket);
        }
        self.waiting.retain(|_, q| !q.is_empty());
    }
}

#[derive(Default)]
struct Fair {
    line: Mutex<Line>,
    changed: Condvar,
}

/// Every server's line, created on first use.
#[derive(Default, Clone)]
pub struct Queues(Arc<Mutex<HashMap<String, Arc<Fair>>>>);

/// A request running on a server; dropping it lets the next one in.
pub struct Running {
    fair: Arc<Fair>,
    limit: u64,
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut line = self
            .fair
            .line
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        line.running = line.running.saturating_sub(1);
        line.admit(self.limit);
        self.fair.changed.notify_all();
    }
}

impl Queues {
    /// Waits for `app`'s turn on `server` until `deadline`. `Ok(None)`
    /// when there is no limit; `Err` with how many were ahead when the
    /// deadline passed.
    pub fn enter(
        &self,
        server: &str,
        app: &str,
        limit: u64,
        deadline: Instant,
    ) -> Result<Option<Running>, usize> {
        if limit == 0 {
            return Ok(None);
        }
        let fair = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(server.to_string())
            .or_default()
            .clone();
        let mut line = fair.line.lock().unwrap_or_else(PoisonError::into_inner);
        let ticket = line.join(app);
        line.admit(limit);
        loop {
            if let Some(i) = line.admitted.iter().position(|&t| t == ticket) {
                line.admitted.swap_remove(i);
                drop(line);
                return Ok(Some(Running { fair, limit }));
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                line.leave(ticket);
                let ahead = line.waiting.values().map(VecDeque::len).sum::<usize>()
                    + usize::try_from(line.running).unwrap_or(usize::MAX);
                return Err(ahead);
            };
            line = fair
                .changed
                .wait_timeout(line, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_go_round_robin_across_apps_and_in_order_within_one() {
        let mut line = Line::default();
        let a1 = line.join("batch");
        let a2 = line.join("batch");
        let a3 = line.join("batch");
        let b1 = line.join("chat");
        let mut order = Vec::new();
        for _ in 0..4 {
            line.admit(1);
            order.append(&mut line.admitted);
            line.running -= 1;
        }
        assert_eq!(order, [a1, b1, a2, a3]);
    }

    #[test]
    fn a_ticket_that_gives_up_leaves_the_line() {
        let mut line = Line::default();
        let t = line.join("a");
        line.leave(t);
        line.admit(1);
        assert_eq!(line.admitted, Vec::<u64>::new());
        assert!(line.waiting.is_empty());
    }

    #[test]
    fn a_full_server_makes_the_next_request_wait_until_one_finishes() {
        let queues = Queues::default();
        let far = Instant::now() + std::time::Duration::from_secs(5);
        let first = queues.enter("s", "a", 1, far).unwrap();
        let soon = Instant::now() + std::time::Duration::from_millis(50);
        assert_eq!(queues.enter("s", "b", 1, soon).err(), Some(1));
        let q = queues.clone();
        let waiter = std::thread::spawn(move || q.enter("s", "b", 1, far).map(|r| r.is_some()));
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(first);
        assert_eq!(waiter.join().unwrap(), Ok(true));
        assert!(matches!(queues.enter("s", "a", 0, far), Ok(None)));
    }
}
