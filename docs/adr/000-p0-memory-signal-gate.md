# ADR-000: P0 feasibility gate for measured-memory admission

- Status: **Proposed**. The gate has not run yet; results are appended below when it has.
- Date: 2026-10-14

## Context

hearth will only admit a model load or request if it fits. That decision is worth something only if the kernel's own numbers predict what happens next. Two in-house predecessors failed on exactly this:

- They read `os.freemem()` on macOS. That counts only truly free pages, so a 64 GB machine looked permanently critical and evicted models in a loop.
- They failed open when they could not measure, so the next load OOMed.

No gateway, scheduler or supervisor code is written until the signals are shown to work on real runtimes.

## Decision

Validate with `hearth-probe` and `scripts/p0-gate.sh` on the primary development machine (Apple M4 Max, 64 GiB), loading real llama-server instances. Run three model sizes, each with and without `--no-mmap`.

Signals sampled at 10 Hz:

- Host: `host_statistics64` (free, active, inactive, speculative, wired, purgeable, internal, external, compressor), `kern.memorystatus_level`, `kern.memorystatus_vm_pressure_level`, `vm.swapusage`.
- Per process: `ri_phys_footprint`, `ri_resident_size`, `ri_wired_size`, `ri_lifetime_max_phys_footprint`.
- Device: Metal `recommendedMaxWorkingSetSize`, `iogpu.wired_limit_mb`.
- llama.cpp's own buffer accounting, from its log (model, KV and compute buffers).

These are experiment thresholds, not product constants. All must pass:

| id | criterion |
|---|---|
| a | Per-process footprint is within 10% (median) / 15% (worst) of what llama.cpp says it allocated |
| b | The footprint change explains the host "used" change within max(10%, 1 GiB) |
| c | Available memory falls by ≥80% of the footprint on load, and recovers ≥90% within 30 s of unload |
| d | Idle noise (3σ) is small relative to the reserve floor (memsize − Metal working set) |
| e | Under induced pressure, a signal trips at least 2 ticks before swap grows or latency doubles |
| f | Free pages alone misread the machine (reproduces the old governor bug) |
| g | Predicted admission refuses 100% of real overloads and wrongly refuses none of the loads with ≥2 GiB true headroom |

If the gate fails, admission falls back to learned per-model footprints with serialized loads, and this ADR records why.

## Observations before the gate

These come from passive sampling of a running llama-server on 2026-10-14:
- Metal `recommendedMaxWorkingSetSize` is 55,662,788,608 bytes (81% of RAM).
- `iogpu.wired_limit_mb` is 0.
- `ri_phys_footprint` reads 30.0 GB with a lifetime peak of 48.1 GB.
- `ri_wired_size` reads 0, so GPU memory is not visible as per-process wired.
- The machine was under memory pressure: level 2 (warn), swap 15.5 of 16.1 GB.
- `proc_pidpath` failed for that server because Homebrew had replaced its binary while it ran, so process matching also uses `proc_name`.

## Results

Pending.
