# Changelog

Entries are timestamped (UTC+08) with the commit time. Versions follow semver once v0.1.0 ships.

## Unreleased

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

- Cargo workspace (edition 2024, MSRV 1.88), Apache-2.0 OR MIT.
- `hearth-sense`: host memory, per-process footprint, process list and accelerator limits.
  - macOS: `host_statistics64`, `sysctlbyname`, `proc_pid_rusage` v4, `proc_listallpids`/`proc_name`/`proc_pidpath`, Metal `recommendedMaxWorkingSetSize`.
  - Linux: `/proc/meminfo`, PSI, cgroup v2, `smaps_rollup`, `/proc/*/comm` and `exe`.
  - No elevated privileges needed.
- `hearth-probe`: `sample` records JSON lines, `mark` adds timestamped markers, `report` evaluates a gate recording.
- `scripts/p0-gate.sh`: drives real llama-server loads for the feasibility gate.
- CI on macOS arm64, Linux x86_64 and Linux arm64, plus an MSRV check.
