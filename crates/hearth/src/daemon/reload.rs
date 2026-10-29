//! `hearth reload`: making the kept servers match the configuration again.

use std::collections::BTreeSet;

use super::{
    Arc, AtomicBool, BTreeMap, Desk, Kept, Order, Ordering, Server, Value, json, keep, lock, log,
};

/// Clears the flag when dropped.
struct Done<'a>(&'a AtomicBool);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What `reload` will change: servers to add, to remove, and to replace.
pub(super) fn plan_reload(
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
pub(super) fn reload(desk: &Arc<Desk>) -> Value {
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
            // Cleared however this ends, a panic included: a reload that
            // stays "in progress" refuses every later one.
            let _done = Done(&worker.reloading);
            let mut stopped = BTreeSet::new();
            for (name, k) in leaving {
                if k.thread.join().is_err() {
                    log(&format!("[{name}] its supervising thread had failed"));
                }
                if k.halted.load(Ordering::SeqCst) {
                    stopped.insert(name.clone());
                }
                lock(&worker.board).remove(&name);
                log(&format!("[{name}] left supervision"));
            }
            for name in starting {
                let Some(server) = config.servers.get(&name) else {
                    continue;
                };
                match keep(
                    &name,
                    server,
                    &config,
                    &worker.board,
                    stopped.contains(&name),
                ) {
                    Ok(k) => {
                        worker.kept().insert(name.clone(), k);
                        log(&format!("[{name}] supervised"));
                    }
                    Err(e) => log(&e),
                }
            }
        });
    match spawned {
        Ok(_) => answer,
        Err(e) => done(json!({
            "error": format!("the reload could not run ({e}); the servers taken out were told to stop; run hearth reload again"),
        })),
    }
}
