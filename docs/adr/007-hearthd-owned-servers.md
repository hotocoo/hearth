# ADR-007: `hearthd` keeps the configured servers alive

- Status: **Accepted**
- Date: 2026-10-15

## Context

`hearth run` keeps one server alive in a terminal (ADR-004). A machine running several model servers, such as chat, vision, embeddings and rerank, needed one terminal per server. Nothing was left to ask "what is up, and what restarted" except reading each terminal.

The plan's daemon also proxies inference and admits work from measured memory. Those wait for the P0 gate (ADR-000), which the user has deferred. Ownership needs neither.

## Decision

### What hearthd does

`hearthd` reads `[servers.NAME]` tables from the user's configuration and keeps each server alive. Each table has:
- `port`;
- `command`, as a list;
- optionally `host` and `expect`.

None are built in, and an empty `[servers]` is an error with the fix in the message.

Each server runs on its own thread with the same `Supervised` type as `hearth run`, so it gets the same:
- preflight;
- restart budget and back-off;
- wedge and poison detection;
- tether;
- journal records (kind `run`, now with a `server` field);
- notifications.

Configuration rejects:
- names that are not letters, digits, `-` and `_`;
- servers without a command;
- two servers on the same address and port.

hearthd proxies no inference and admits nothing: that stays behind the gate.

### Control socket

`hearthd.sock` lives in hearth's state directory, which hearthd makes owner-only (0700) before binding. Who may talk to hearthd is decided by file permissions: there is no TCP port to expose and no key to leak. The protocol is one JSON request and one JSON answer per line.

- `status`: each server's name, address, expected model, program, state (`starting`, `ready`, `waiting`, `gave_up`), pid, restart count and last event.
- `restart NAME`: stop and start again at once, without charging the restart budget, because a person asking is not a crash. It also revives a server hearthd gave up on. Unknown names are refused.

`hearth status`, `hearth restart NAME` and the MCP tools `status` and `restart` are clients of this socket. The answer comes from hearthd's memory, so the wait is bounded by `discovery.first_contact_timeout_ms`.

A socket path longer than the platform allows (about a hundred bytes) is reported with its length and the fix (`HEARTH_STATE`), not the kernel's bare `SUN_LEN`.

### Lifetime

hearthd has no shutdown handler, and needs none. However hearthd ends, SIGKILL included, the kernel closes each tether's pipe and the servers stop (ADR-004).
- **Stale socket.** A socket left behind does not answer, so the next hearthd replaces it.
- **Second instance.** A socket that answers means hearthd already runs, and a second instance refuses to start.

## Consequences

- The end-to-end test:
  - starts the built `hearthd`, with a real server;
  - waits for `hearth status` to report it ready;
  - restarts it through `hearth restart` and sees a new pid;
  - SIGKILLs hearthd, sees the server's port close, and finds the requested restart in `hearth log --kind run`.
- Live on the development machine, a steady and a crash-looping server ran side by side. `status` showed one ready and one `gave_up` after its budget, and MCP `status` returned the same.
- An agent can now restart a wedged server it was asked to look after, through MCP, without being able to touch anything else on the machine.
- The `hearth` crate is now a library with two binaries, `hearth` and `hearthd`, so both share one implementation of every command.
