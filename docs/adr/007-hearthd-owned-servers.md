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

`hearthd.sock` lives in hearth's state directory. Who may talk to hearthd is decided by ownership and permissions, checked on both ends. There is no TCP port to expose and no key to leak.

- **Ownership.** The directory must be a real directory (not a symlink), owned by the effective user, with no group or other access. hearthd creates it 0700 when missing, but never changes an existing one: `HEARTH_STATE=$HOME` is refused with the fix, not silently chmodded. The client checks the same, and that the socket itself is owner-only, before trusting an answer. A spoofed socket in a shared directory such as `/tmp` is refused.
- **Lock.** One hearthd per state directory. It holds an exclusive lock on `hearthd.lock` (`File::try_lock`, the reason MSRV is 1.89) for as long as it runs, and the kernel drops the lock however hearthd ends. A socket found without the lock held is therefore stale, and it is replaced. Only a socket is ever removed from that path; anything else there is an error.
- **Socket.** The socket is created 0600.
- **Bounds.** One JSON request and one JSON answer per line, each at most `control.max_message_bytes`, the exchange within `control.timeout_ms`. Each client gets its own handler, up to `control.max_clients` at once. A client beyond that is answered "busy" straight away, so stalled clients can occupy handlers for at most the timeout, but never memory or threads without bound. (A first version answered clients one at a time; on CI a stalled client then held the next one past its own deadline.) A malformed or oversized request gets an error answer. Text printed by `hearth status` is stripped of control characters.

- `status`: each server's name, address, expected model, program, state (`starting`, `ready`, `waiting`, `gave_up`), pid, restart count and last event.
- `restart NAME`: stop and start again at once, without charging the restart budget, because a person asking is not a crash. It also revives a server hearthd gave up on. Unknown names are refused. At most one order is pending per server, so repeated requests cannot hold a server down.

`hearth status`, `hearth restart NAME` and the MCP tools `status` and `restart` are clients of this socket. The answer comes from hearthd's memory, so the wait is bounded by `discovery.first_contact_timeout_ms`.

A socket path longer than the platform allows (about a hundred bytes) is reported with its length and the fix (`HEARTH_STATE`), not the kernel's bare `SUN_LEN`.

### Robustness

- hearthd writes its log without panicking when stdout is closed.
- A supervising thread that panics marks its server `unsupervised` on the board.
- A poisoned board lock is recovered: the board holds plain values.
- Accept errors (out of descriptors) are logged and paced, not spun on.

### Lifetime

hearthd has no shutdown handler, and needs none. However hearthd ends, SIGKILL included, the kernel closes each tether's pipe and the servers stop (ADR-004).
- **Stale socket.** A socket left behind is replaced, because the lock says nobody owns it.
- **Second instance.** It finds the lock held and refuses to start.

### Running at login

`hearth service install|uninstall|show` puts hearthd under the platform's own service manager, as the user:
- **macOS:** a LaunchAgent with `RunAtLoad` and `KeepAlive`, logging to the state directory.
- **Linux:** a systemd user unit with `Restart=on-failure` and `KillMode=process`. Only hearthd is signalled; its servers stop through their tethers.

The rules:
- The unit names the `hearthd` beside the running `hearth`, and carries this shell's `HEARTH_CONFIG`, `HEARTH_STATE` and `PATH`, quoted for the unit's syntax. Service managers start agents with a minimal `PATH`, in which `llama-server` or a Homebrew `python3` would not be found.
- `install` refuses when no servers are configured. The service manager would otherwise restart a hearthd with nothing to keep, forever.
- `service` is not an MCP tool: installing something that runs at every login is for a person to decide.

## Consequences

- The end-to-end test:
  - starts the built `hearthd`, with a real server;
  - waits for `hearth status` to report it ready;
  - restarts it through `hearth restart` and sees a new pid;
  - checks that a second hearthd refuses to start;
  - checks that an oversized request gets an error, and that a stalled client does not block the next one;
  - SIGKILLs hearthd, sees the server's port close, and finds the requested restart in `hearth log --kind run`.

  Separate tests check that a regular file at the socket path is left untouched, and that a state directory open to other users is refused.
- Live on the development machine, a steady and a crash-looping server ran side by side. `status` showed one ready and one `gave_up` after its budget, and MCP `status` returned the same.
- An agent can now restart a wedged server it was asked to look after, through MCP, without being able to touch anything else on the machine.
- The `hearth` crate is now a library with two binaries, `hearth` and `hearthd`, so both share one implementation of every command.
