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
- optionally `host` and `expect`;
- optionally `env`, variables set on top of hearthd's own (`GGML_METAL_*`, `CUDA_VISIBLE_DEVICES`, `HF_HOME`), and `dir`, the absolute working directory. A relative program path is taken relative to `dir`. `hearth bundle` redacts every `env` value, since tokens go there.

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

- `status`: each server's name, address, expected model, program, state (`starting`, `ready`, `waiting`, `stopped`, `gave_up`, `unsupervised`), restart count and last event.
  - `pid`: the tether's pid.
  - `server_pid`: the process listening on the server's port inside the tether's process group, which is the server however it was wrapped. The lowest pid wins for pre-forking servers. It is never another process using the same port. It is learned when the server becomes ready and forgotten when it goes, so a reused pid is never measured.
  - `footprint_bytes`: that process's footprint, measured per request, or `footprint_error` when it cannot be measured.
- `restart NAME`: stop and start again at once, without charging the restart budget, because a person asking is not a crash. It also revives a server hearthd gave up on. Unknown names are refused. Orders do not queue: the latest one for a server replaces any not yet acted on, so `stop` then `start` means start, and repeated requests cannot hold a server down.

- `stop NAME`: stop the server and keep it down, to free its memory, until `start NAME`. A stop is not a failure: nothing is restarted, charged or notified, and status shows `stopped`.
- `start NAME`: start a stopped server, or one hearthd gave up on. Starting one that runs does nothing. A start or restart forgets the crash ledger, because a person asking is a new beginning; without that, a server started right after a give-up would give up again at its first exit.
- `logs NAME`: the last `supervise.output_lines` lines the server printed on stdout and stderr, across restarts.
  - Each line is cut at `supervise.output_line_bytes`, and output without newlines is taken in pieces, so memory stays bounded.
  - An answer that would exceed `control.max_message_bytes` drops its oldest lines and says how many (`left_out`). These are what explain a load failure, and agents should not have to hunt for log files to find them.

- `reload`: re-read the configuration and make the kept servers match its `[servers]`, without restarting hearthd:
  - servers added to the file start;
  - removed ones stop;
  - changed ones are stopped and, once the old process is gone (it holds the port until then), started with the new definition;
  - every other server keeps running, with the same process.

  A configuration that does not load changes nothing.
  - One reload runs at a time; another is refused until the first has started its last server.
  - Planning and taking servers out of service happen under one lock, and every leaving server is told to stop at once, so stops overlap.
  - During a reload, orders for a server being replaced are refused with "try again" rather than lost. Other sections take effect when hearthd restarts. The answer lists what is added, removed and changed; the stopping and starting happen after it, since a stop can take the grace.

`hearth status`, `hearth restart|stop|start|logs NAME`, `hearth reload` and the MCP tools of the same names are clients of this socket. The answer comes from hearthd's memory, so the wait is bounded by `discovery.first_contact_timeout_ms`.

A socket path longer than the platform allows (about a hundred bytes) is reported with its length and the fix (`HEARTH_STATE`), not the kernel's bare `SUN_LEN`.

### Metrics

With `metrics.listen` set (off by default), hearthd serves Prometheus metrics at `GET /metrics`. Each scrape gets its own handler, up to `control.max_clients`, and its request is read within the control socket's size limit and total deadline, so a stalled scraper cannot hold the others up. The metrics are:
- `hearth_server_state{server,state}`: one series per server and possible state, 1 for the current one;
- `hearth_server_restarts_total{server}`;
- memory total, available and swap used; kernel pressure (0 normal, 1 warn, 2 critical);
- open files and threads, with the kernel's limits.

Machine values are measured at each scrape. Labels are bounded: server names from the configuration and a fixed set of states. The endpoint is read-only and says nothing `hearth status` and `hearth doctor` do not, but it names the servers, so the documented example listens on loopback.

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
