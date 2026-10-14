# Changelog

Entries are timestamped (UTC+08). Versions follow semver once v0.1.0 ships.

## Unreleased

### 2026-10-14 22:15 P0 scaffold

- Cargo workspace (edition 2024, MSRV 1.88), Apache-2.0 OR MIT.
- `hearth-sense`: host memory, per-process footprint, process list and accelerator limits.
  - macOS: `host_statistics64`, `sysctlbyname`, `proc_pid_rusage` v4, `proc_listallpids`/`proc_name`/`proc_pidpath`, Metal `recommendedMaxWorkingSetSize`.
  - Linux: `/proc/meminfo`, PSI, cgroup v2, `smaps_rollup`, `/proc/*/comm` and `exe`.
  - No elevated privileges needed.
- `hearth-probe`: `sample` records JSON lines, `mark` adds timestamped markers, `report` evaluates a gate recording.
- `scripts/p0-gate.sh`: drives real llama-server loads for the feasibility gate.
- CI on macOS arm64, Linux x86_64 and Linux arm64, plus an MSRV check.
