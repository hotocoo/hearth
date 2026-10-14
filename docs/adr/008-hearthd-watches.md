# ADR-008: hearthd watches the machine as well

- Status: **Accepted**
- Date: 2026-10-15

## Context

`hearth watch` (ADR-006) observes and reports, but only while someone keeps it running in a terminal. hearthd (ADR-007) is the one process meant to run all the time, and it kept servers alive without looking at the machine around them. Memory pressure that will kill those servers, and leaks that will exhaust the box, went unseen unless someone also ran `hearth watch`.

## Decision

- **hearthd runs the watch.** When `watch.in_hearthd` is true (the default), hearthd runs the same watch loop on its own thread: the same rounds, journal records, problem tracking and `[notify]` deliveries. Its findings go to hearthd's log, prefixed by source like `hearth watch` prints them.
- **One watcher.** `hearth watch` refuses to run while hearthd holds `hearthd.lock` with watching on, and says where the findings are. Two watchers would record every round twice and notify every problem twice. The check takes a shared lock for an instant and never keeps it. Setting `watch.in_hearthd = false` hands watching back to `hearth watch`.
- **Logging.** Inside hearthd a closed stdout loses the log, never the watching. The command stops when its output closes, because its reader is gone.

## Consequences

- A machine with hearthd installed as a login service (`hearth service install`) is watched from login on. Every doctor and ports round is in the journal, so `hearth diff` can always answer "what changed since this morning".
- Live, hearthd logged kernel memory pressure **warn** in its first round, alongside starting its server. A `hearth watch` started beside it was refused with the reason.
- Rounds run one after another on the watch thread. A ports round, which waits out the deadline of every silent listener, can delay the next doctor round by that long.
