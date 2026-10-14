# Changelog

Entries are timestamped (UTC+08) with the commit time. Versions follow semver once v0.1.0 ships.

## Unreleased

### 2026-10-15 01:35 fit --port

- `hearth fit --port P --ctx N` (MCP `fit` with `port`) fits the model a running server serves: llama.cpp's file, or Ollama's loaded model. It answers whether that server could take a different context, without touching it.

### 2026-10-15 01:32 Linux incidents across reboots; prompts/list per spec

- `hearth incidents` on Linux reads the kernel log of every boot the journal kept (`journalctl _TRANSPORT=kernel`). It used `-k`, which implies the current boot only, so the OOM kill before a reboot was missed. On the Linux CI runner the difference is 542 kernel lines against 536.
- `prompts/list` sends only the argument fields MCP defines; defaults are stated in each argument's description.

### 2026-10-15 01:27 MCP prompts: diagnostic workflows

- `hearth mcp` serves four workflows through `prompts/list` and `prompts/get`: `diagnose-slow-machine`, `server-died`, `can-i-load` and `what-changed`.
- Each workflow names the tools to call, in order, and how to answer.
- Arguments have defaults, and a missing required argument is an invalid-params error.

### 2026-10-15 01:24 Ollama model names in fit

- `hearth fit qwen3:8b …` and MCP `fit` with `model: "qwen3:8b"` find the weights in Ollama's store, read from `OLLAMA_MODELS` or Ollama's defaults. Only files are read: Ollama need not run, and nothing is loaded.
- MCP tool descriptions for `fit` and `incidents` now cover Ollama names and Linux.

### 2026-10-15 01:20 Linux incidents

- `hearth incidents` on Linux reads the kernel log with `journalctl -k`. It reports:
  - OOM kills: the process, its pid, the anonymous memory freed, and whether a cgroup limit did it;
  - hung tasks: the process, its pid and how long it was stuck.
- When the kernel log cannot be read, the command reports why.
- CI smoke-tests the command on the Linux runners.

### 2026-10-15 01:17 supply-chain checks in CI

- `cargo deny check` runs on every push. Licenses must be compatible with Apache-2.0 OR MIT. Yanked crates, wildcard versions, unknown registries and git sources fail the build, as do RustSec advisories.

### 2026-10-15 01:15 hearthd watches the machine (ADR-008)

- With `watch.in_hearthd = true` (new key, default), hearthd runs the `hearth watch` rounds on its own thread. Findings go to its log, the journal and `[notify]`.
- `hearth watch` refuses to run while hearthd watches, and says where the findings are.
- Inside hearthd a closed stdout never stops the watching.

### 2026-10-15 01:13 login service PATH and control handler fixes

- `hearth service install` carries `PATH` into the unit. Service managers start agents with a minimal one, in which `llama-server` would not be found.
- hearthd serves each control client on its own handler, up to `control.max_clients`; a stalled client no longer holds the next one past its deadline. A handler that panics gives its slot back.
- `hearth_core::state::ensure_dir` creates the state directory owner-only.

### 2026-10-15 01:06 hearthd control socket hardening

- **Lock.** One hearthd per state directory, enforced with an exclusive lock on `hearthd.lock`. A stale socket is replaced only when it is a socket. The socket is 0600. MSRV is now 1.89, for `File::try_lock`.
- **Ownership.** The state directory and socket must be real, owned by the user, and private. hearthd and the client both check, so a spoofed socket in a shared directory is refused. An existing directory is never chmodded.
- **Limits.** New `[control]` keys `max_message_bytes`, `timeout_ms` and `max_clients` bound every exchange and the number of clients served at once. Beyond that number, a client is answered "busy". Bad requests get an error answer.
- **Restarts.** A restart is acted on at once, and at most one order is pending per server.
- **Resilience.** Logging survives a closed stdout. A panicking supervisor marks its server `unsupervised`. Poisoned locks recover. Accept errors are paced.
- **Status.** Control characters are stripped from `hearth status` output.
- **Tests.** New tests cover a second hearthd, oversized and stalled clients, a file squatting the socket path, and a state directory open to others.

### 2026-10-15 00:56 hearth service

- `hearth service install|uninstall|show`: runs hearthd at login as the user.
  - macOS: a LaunchAgent with KeepAlive.
  - Linux: a systemd user unit with `KillMode=process`; servers stop through their tethers.
- The unit carries `HEARTH_CONFIG` and `HEARTH_STATE` from the installing shell.
- `install` refuses with no servers configured.
- Not an MCP tool.

### 2026-10-15 00:53 hearthd, hearth status and restart (ADR-007)

- `hearthd` keeps every `[servers.NAME]` from the user's configuration alive, each exactly as `hearth run` would. A server is given by `port`, `command`, and optionally `host` and `expect`. Names, commands and port clashes are validated.
- Control socket `hearthd.sock`, in the owner-only state directory, with one JSON line per request:
  - `hearth status` (MCP `status`): state, pid, restarts and last event per server.
  - `hearth restart NAME` (MCP `restart`): restart now without charging the budget, also after a give-up. Unknown names are refused.
- A second hearthd refuses to start; a stale socket is replaced. A socket path that is too long is explained with the fix.
- `run` journal records carry the server's name.
- The MCP tool list lives in `mcp-tools.json`.
- The `hearth` crate is a library with the `hearth` and `hearthd` binaries.
- End-to-end test of the built binaries, ending with SIGKILL of hearthd and its server's port closing.

### 2026-10-15 00:45 watch and notify hardening

- **Memory pressure.** Keyed by level: warn rising to critical is reported, and the warn clears.
- **Clears.** Clears are notified as well as printed.
- **Failed rounds.** A round that fails (journal, process table) is reported once, and the watch continues.
- **Confirmation.** A projected limit must hold for `watch.confirm_rounds` (new key) rounds in a row to be reported, and be gone as long to clear.
- **Runtimes.** Any runtime seen during a watch stays known, so silent-then-vanished and answering-again are reported.
- **Delivery.** A delivery is killed after `notify.timeout_ms`. curl gets the URL and body on stdin, never on the command line, with `-q` and `--globoff`.
- **Run events.** They keep only the program, not its arguments, which can hold keys.
- **Exit.** `hearth run` waits for pending deliveries on every exit path.

### 2026-10-15 00:37 hearth config

- `hearth config [FILTER]` and MCP `config`: every setting in effect, with its value, its source (built in, the user's file, or unset) and the shipped file's explanation of it.
- A test fails when a key in `config/hearth.toml` has no comment, so every number hearth uses stays explained.

### 2026-10-15 00:33 hearth watch (ADR-006)

- `hearth watch [--for-seconds N]` runs `doctor` and `ports` on the `[watch]` intervals. Every round is recorded, so `log` and `diff` cover the time watched.
- A problem is reported once when it appears and once when it clears, on stdout (JSON lines with `--json`) and through `[notify]`. Problems are:
  - memory pressure the kernel declares;
  - a resource that would reach its limit within `watch.limit_horizon_ms` at the measured rate;
  - a model runtime that vanished, restarted or stopped answering.
- Doctor findings carry a stable `code` and `pid`.
- Configuration validation is split by concern, and the watch intervals are checked like the others.

### 2026-10-15 00:25 run events in the journal, notifications (ADR-005)

- Every `hearth run` event is recorded in the journal as kind `run`, with the address, expected model and command. `hearth log --kind run` lists them, and MCP `log` takes `kind: run`.
- `[notify]` configuration, both channels off by default:
  - `desktop` uses `osascript` on macOS and `notify-send` on Linux. Text is passed as arguments, never as script.
  - `webhook_url` POSTs the event as JSON with `curl` (http or https), bounded by `timeout_ms`.
- Notifications fire on a poisoned backend, a wedge, a lost process, and any give-up.
- A failed delivery is reported on stderr and never affects supervision. Pending deliveries finish before `hearth run` exits.

### 2026-10-15 00:23 restart at the moment Metal latches

- The poison signature is now `ggml_metal_synchronize: error: command buffer … failed`, the line llama.cpp logs where it sets `has_error`. The request that hit the failure is the last to fail, one sooner than before. The capture path's INFO line does not latch and is not matched.
- Durations the platform clock cannot represent are rejected in `[supervise]`.

### 2026-10-15 00:18 poisoned backends restarted from their own log

- `hearth run` reads the server's stdout and stderr line by line. The lines are passed on to hearth's stderr; hearth's stdout carries only events.
- **Poisoned.** A server is stopped and restarted when its log says the backend is permanently broken. On Metal that is llama.cpp's "backend is in error state from a previous command buffer failure - recreate the backend to recover". Such a server still answers `/health` 200, and fails every request in milliseconds until restarted.
- **Noticed.** Accelerator out-of-memory lines (Metal, CUDA, Vulkan) are reported as `noticed` events.
- `hearth-runtime::signals`: log signatures taken from the runtimes' own format strings.

### 2026-10-15 00:14 hearth run: supervised owned servers (ADR-004)

- `hearth run --port P [--expect NAME] -- <command>` starts a model server and keeps it alive.
  - **Preflight.** Before every start, the port is checked. If the expected model already answers, nothing is started. Anything else is refused by name, and on a restart it counts as a failed start.
  - **Restarts.** On exit there is a back-off restart. More than the budget of exits inside one window is a crash loop: hearth gives up and exits 1. A server ready a whole window earns the shortest back-off back.
  - **Readiness.** A server not ready within the timeout is stopped and restarted. Ready means the port answers as a runtime with the expected model and no loading state.
  - **Wedges.** A ready server whose port stops serving the expected model for several probes in a row is called wedged, stopped and restarted.
  - **Tether.** The server runs in its own process group. When hearth stops, exits or is killed with SIGKILL, the group gets SIGTERM, then SIGKILL after `stop_grace_ms`. That covers wrapper scripts and latched backends that ignore SIGTERM.
  - **Output.** Events are human lines, or JSON lines with `--json`. A closed output pipe stops the server cleanly.
  - **Commands.** A missing command is reported before anything starts.
- New `[supervise]` configuration:
  - keys: `restart_budget`, `restart_window_ms`, `backoff_start_ms`, `backoff_max_ms`, `backoff_factor`, `ready_timeout_ms`, `stop_grace_ms`, `probe_interval_ms`, `unresponsive_probes`;
  - validated, including that the maximum back-off fits inside the restart window.
- `hearth-runtime::supervise`: the supervisor as a library type, for the daemon.

### 2026-10-14 23:35 journal, hearth log and diff, learned probe deadlines (ADR-003)

- Full runs of `ports` and `doctor` are recorded in hearth's journal: `$HEARTH_STATE`, else `$XDG_STATE_HOME/hearth`, else `~/.local/state/hearth`, created mode 0700.
  - One JSON file per snapshot, written to a temporary file and renamed, so concurrent runs are safe.
  - `state.max_snapshots_per_kind` are kept per kind.
- `hearth log`: snapshots newest first, each with a one-line summary.
- `hearth diff`:
  - ports: listeners that appeared or vanished, runtimes that restarted, stopped answering or changed models or health, and footprint movement;
  - doctor: pressure changes, available memory, swap, kernel-wide and per-process open files, threads and `CLOSE_WAIT`.
  - Snapshots are referenced by id or `~N`.
  - `--limit` applies per unit.
- MCP tools `log` and `diff`. Snapshot references from clients are validated before they reach the file system.
- `ports` learns each program's probe deadline from its recorded answers. A runtime that misses its learned deadline is asked again, then reported `slow`. Every port reports its `deadline` and its source.
  - New `[discovery]` knobs: `learned_deadline_multiplier`, `learned_min_samples` and `learned_deadline_floor_ms`.
  - Answer times are now microseconds (`latency_us`).
- Configuration values that parse but mean nothing (multiplier below 1, zero samples, zero retention, floor above timeout) are rejected with the key and the reason.

### 2026-10-14 22:46 hearth ports, layered config (ADR-002)

- `hearth ports` and MCP `ports`: every listening TCP port with its process, identified by what answers there.
  - Runtimes recognised: Ollama (models, loaded sizes), llama.cpp (model, n_ctx, slots, `/health`), ComfyUI, SD WebUI API, LM Studio, OpenAI-compatible.
  - Silent ports are flagged.
  - Footprint is shown for runtime processes.
- `hearth-runtime`: bounded HTTP/1.1 probe client and fingerprinting.
- `hearth-core`: layered configuration. `config/hearth.toml` is embedded as the base, the user file is merged over it, and unknown keys are rejected.
- `doctor` lists CLOSE_WAIT holders and listening ports per process; `--watch` projects CLOSE_WAIT growth.

### 2026-10-14 22:36 doctor thread total, provisional fit

- `doctor` reads the system-wide thread total from the scheduler. On macOS that is `processor_set_statistics`, the figure `top` shows; on Linux it is `/proc/loadavg`. It also reports how many threads sit in processes owned by other users. Before this, those threads were silently missing from the count; they are the processes that matter in a watchdog panic.
- `fit` marks "fits right now" as provisional (`fits_now_provisional`) until the P0 gate confirms how memory-mapped weights show up in available memory.
- `p0-gate.sh`: the readiness poll uses `INTERVAL_MS`, and the fill timeout is a required `FILL_TIMEOUT_S`.
- CHANGELOG times corrected to the actual commit times.

### 2026-10-14 22:32 hearth mcp

- `hearth mcp` serves the CLI commands as MCP tools over stdio: `doctor`, `incidents`, `fit` and `process_memory`.
  - It speaks protocol revisions 2024-11-05 to 2025-11-25.
  - Tool arguments are parsed by the same `clap` definitions as the CLI, so agents and people get identical validation and answers.
  - Errors are returned in-band with `isError`.

### 2026-10-14 22:30 hearth CLI: doctor, incidents, fit (ADR-001)

- `hearth doctor`:
  - reports memory, open files and threads against the kernel's limits;
  - names the processes holding half of each resource;
  - `--watch` projects time to each limit from measured growth;
  - each finding carries a `remediation`.
- `hearth incidents` classifies macOS kernel panics (watchdog, GPU memory driver, other), jetsam kills, resource-limit reports and shutdown stalls.
- `hearth fit`:
  - weights plus KV bytes from GGUF metadata;
  - checks against the Metal working set, `iogpu.wired_limit_mb`, available memory and pressure;
  - reports max context, capped at the trained context.
- `--json` on every command.
- `hearth-sense`:
  - bounded GGUF reader;
  - KV geometry (full, sliding, latent, recurrent; cache types f32 to iq4_nl);
  - per-process open-file and thread counts;
  - system limits.
- No hardcoded values:
  - probe interval and gate parameters are required;
  - report thresholds live in `docs/adr/000-criteria.json`;
  - sampling windows are derived from phase marks.

### 2026-10-14 22:12 P0 scaffold

- Cargo workspace (edition 2024, MSRV 1.88, now 1.89), Apache-2.0 OR MIT.
- `hearth-sense`: host memory, per-process footprint, process list and accelerator limits.
  - macOS: `host_statistics64`, `sysctlbyname`, `proc_pid_rusage` v4, `proc_listallpids`/`proc_name`/`proc_pidpath`, Metal `recommendedMaxWorkingSetSize`.
  - Linux: `/proc/meminfo`, PSI, cgroup v2, `smaps_rollup`, `/proc/*/comm` and `exe`.
  - No elevated privileges needed.
- `hearth-probe`: `sample` records JSON lines, `mark` adds timestamped markers, `report` evaluates a gate recording.
- `scripts/p0-gate.sh`: drives real llama-server loads for the feasibility gate.
- CI on macOS arm64, Linux x86_64 and Linux arm64, plus an MSRV check.
