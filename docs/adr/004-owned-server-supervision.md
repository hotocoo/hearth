# ADR-004: Supervising servers hearth starts (`hearth run`)

- Status: **Accepted**
- Date: 2026-10-14

## Context

The failures in the user's notes recur because nobody owns a model server's life:
- a llama.cpp server whose Metal backend latched after `kIOGPUCommandBufferCallbackErrorOutOfMemory` keeps its port and fails every request until someone restarts it, and it ignores SIGTERM;
- a scheduler that wedges accepts connections and never answers;
- a server left behind by a crashed parent holds its port and its memory;
- two servers started for the same model load it twice.

Aletheia's supervisor (its ADR-240) already solved the restart and tether parts, with tests against real processes. supportdesk's `reapShimProcess` showed that SIGTERM is not enough for a latched backend.

## Decision

`hearth-runtime::supervise` ports Aletheia's supervisor. `hearth run --port P [--expect NAME] -- <command>` drives it.

### Identity before trust

Before every start, the first one and every restart, the port is fingerprinted with the same probe `hearth ports` uses:
- **Nothing listens:** start.
- **A runtime already serves the expected model:** start nothing and exit 0. With no `--expect`, any runtime counts.
- **Anything else answers:** refuse, naming what answered. hearth never touches a server it did not start. On a restart this is a `port_taken` event, counted like an exit, so a port that stays taken ends in giving up rather than a loop.
- `hearth run` checks before the first start that the command exists, as the shell will look for it. Otherwise a typo would start a tether that exits 127 on every restart until the budget is spent.

A started server is ready only when its port answers as a model runtime that:
- reports a model whose name contains `--expect`;
- reports no loading state (llama.cpp `/health`).

### Restarts and wedges

- **Exits.** Back-off starts at `supervise.backoff_start_ms` and grows by `backoff_factor` up to `backoff_max_ms`. It is measured from when the server was really gone, after any stop grace. More than `restart_budget` exits inside `restart_window_ms` is a crash loop: hearth gives up and exits 1. A server that stayed ready a whole window earns the shortest back-off back.
- **Never ready.** A server not ready within `ready_timeout_ms` is stopped and counted as an exit. The event carries what the port last answered.
- **Wedged.** A ready server is stopped and counted as an exit when `unresponsive_probes` probes in a row find its port not serving the expected model. Not serving means no answer, a refused connection, or a different answer. A listener that died inside a live process looks like that, as does a scheduler that accepts connections and never answers. Probes are `probe_interval_ms` apart. One poll blocks for at most one fingerprint: a few requests, each bounded by `discovery.first_contact_timeout_ms`.
- **Poisoned.** The server's stdout and stderr are read line by line and passed on to hearth's stderr; hearth's stdout carries only its own events. A line matching a poison signature stops the server and counts as an exit. On Metal, llama.cpp latches in `ggml_metal_synchronize`, which logs "error: command buffer %d failed with status %d" right where it sets `has_error`. That line triggers the restart, so the request that hit the failure is the last one to fail. Every later request would log "backend is in error state from a previous command buffer failure - recreate the backend to recover", which is matched too, for a server whose first line was missed. The capture path logs a similar line at INFO level without latching, under another function name, and is not matched. Such a server still answers `/health` 200, so probing alone never finds it. This is the failure supportdesk diagnosed by hand. Accelerator out-of-memory lines (Metal `kIOGPUCommandBufferCallbackErrorOutOfMemory`, CUDA, Vulkan) are reported as `noticed` events and change nothing by themselves. Signatures are the runtimes' own format strings, in `hearth-runtime::signals`. Lines from a server that has already been stopped never act on its successor.
- **Lost.** When the operating system cannot say whether the server still runs (`try_wait` fails), the server is stopped and the event carries the error. Errors from stopping are reported on the event that caused the stop.
- **Exit status.** A server killed by a signal is reported as that signal (the shell's `128 + n`), not as an exit code.

The ledger (`Restarts`) has an injected clock, and every rule is unit-tested without processes.

### The tether

On Unix every server runs in a new process group, under a small `sh` tether. A watcher in that group holds the supervisor's end of a pipe on fd 3. When the pipe closes, the watcher sends SIGTERM to the whole group, then SIGKILL after `stop_grace_ms`.

- **When.** The pipe closes when hearth stops the server, exits, or is killed with SIGKILL.
- **Who.** The group covers wrapper scripts that do not `exec` the server, and whatever they started. A server that calls `setsid` leaves the group, and with it hearth's reach.
- **Reuse.** While the watcher lives the group id cannot be reused, so the signals only ever reach processes hearth started.
- **Leftovers.** When a server exits on its own, the pipe is closed too, so the watcher clears whatever the server left in its group.
- **Why SIGKILL.** The escalation is the supportdesk lesson: a latched backend does not exit on SIGTERM.
- **Uninterruptible state.** A process stuck in the kernel, for example a hung GPU driver, can outlast SIGKILL. Stopping then waits with it, since nothing else can be done.

Integration tests run real Python servers that exit, serve the wrong model, wedge, ignore SIGTERM, and run behind a wrapper script. Exits and wedges are triggered by a file the test writes, never by a timer, so slow CI machines cannot race them. The tests check:
- the crash-loop sequence;
- refusal by name, and `port_taken` on a start;
- the never-ready restart;
- wedge detection and kill;
- the tether through a wrapper;
- a server that keeps answering while its log says it is poisoned, which is restarted.

A live run on the development machine:
- `kill -9` of `hearth run` with a wrapper script around the server left the port free within the grace;
- when `hearth run`'s output closes (piped into `head`), hearth stops the server and exits 1. It writes its events instead of printing them, so a closed pipe is an error rather than a panic;
- pointed at the user's llama-server port, `hearth run` reported "already serves …; nothing started" and touched nothing.

### Numbers

Every value is a `[supervise]` key in `config/hearth.toml`. The defaults are Aletheia's. Configuration rejects:
- zero windows, timeouts, probe intervals and probe counts;
- a back-off factor of 0;
- a starting back-off above the maximum;
- a maximum back-off longer than the restart window, because exits would then never share a window and a crash loop would go unnoticed.

### Probe cost

A ready server is fingerprinted in full on every probe: a few status GETs, such as Ollama's `/api/version`, `/api/tags` and `/api/ps`, or llama.cpp's `/props`, `/health` and `/v1/models`. This was weighed against a lighter liveness check. A single request cannot confirm that the expected model is still served, and that is what a wedge or a swapped model looks like. These endpoints answer from the runtimes' HTTP threads without touching inference. `supervise.probe_interval_ms` sets the rate for anyone who wants it lower.

## Consequences

- One server per `hearth run`. The daemon will own many, through the same `Supervised` type, plus the admission ledger once the P0 gate has run.
- `hearth run` refuses to be an MCP tool, because it runs until it gives up. Agents can still see its server through `ports`, and see its restarts through `diff`.
- The port must be given. Detecting it from the child's sockets would need the process tree, which waits for the daemon.
- On non-Unix platforms there is no tether: stopping kills the direct child only.

## Addendum (2026-10-15): why it exited

An `exited` event now says why, when the server's log said so: `cause` is `model_missing`, `load_failed`, `out_of_memory`, `port_in_use` or `bad_argument`, and `cause_line` is the line that said it. The signatures are llama.cpp's own messages (gguf.cpp, llama.cpp, the ggml backends, server.cpp, common/arg.cpp). The first line that matches wins, because what follows is usually its consequence. Lines are read per start, and on exit the supervisor waits for both output streams to reach their end, for at most `supervise.stop_grace_ms`, so the last words before the exit are counted. This is the start of the recovery classifier: a missing model or a bad argument will not be cured by a restart, and the `server-died` workflow reads the cause first.

