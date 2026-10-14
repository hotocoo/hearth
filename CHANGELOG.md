# Changelog

Entries are timestamped (UTC+08) with the commit time. Versions follow semver once v0.1.0 ships.

## Unreleased

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
