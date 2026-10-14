# Changelog

Entries are timestamped (UTC+08). Versions follow semver once v0.1.0 ships.

## Unreleased

### 2026-10-15 00:05 hearth mcp

- `hearth mcp` serves the CLI commands as MCP tools over stdio: `doctor`, `incidents`, `fit` and `process_memory`.
  - It speaks protocol revisions 2024-11-05 to 2025-11-25.
  - Tool arguments are parsed by the same `clap` definitions as the CLI, so agents and people get identical validation and answers.
  - Errors are returned in-band with `isError`.

### 2026-10-14 23:40 hearth CLI: doctor, incidents, fit (ADR-001)

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

### 2026-10-14 22:15 P0 scaffold

- Cargo workspace (edition 2024, MSRV 1.88), Apache-2.0 OR MIT.
- `hearth-sense`: host memory, per-process footprint, process list and accelerator limits.
  - macOS: `host_statistics64`, `sysctlbyname`, `proc_pid_rusage` v4, `proc_listallpids`/`proc_name`/`proc_pidpath`, Metal `recommendedMaxWorkingSetSize`.
  - Linux: `/proc/meminfo`, PSI, cgroup v2, `smaps_rollup`, `/proc/*/comm` and `exe`.
  - No elevated privileges needed.
- `hearth-probe`: `sample` records JSON lines, `mark` adds timestamped markers, `report` evaluates a gate recording.
- `scripts/p0-gate.sh`: drives real llama-server loads for the feasibility gate.
- CI on macOS arm64, Linux x86_64 and Linux arm64, plus an MSRV check.
