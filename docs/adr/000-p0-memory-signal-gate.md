# ADR-000: P0 feasibility gate for measured-memory admission

- Status: **Proposed**. The gate has not run yet; results are appended below when it has.
- Date: 2026-10-14

## Context

hearth will only admit a model load or request if it fits. That decision is worth something only if the kernel's own numbers predict what happens next. Two in-house predecessors failed on exactly this:

- They read `os.freemem()` on macOS. That counts only truly free pages, so a 64 GB machine looked permanently critical and evicted models in a loop.
- They failed open when they could not measure, so the next load OOMed.

No gateway, scheduler or supervisor code is written until the signals are shown to work on real runtimes.

## Decision

Validate with `hearth-probe` and `scripts/p0-gate.sh` on the primary development machine (Apple M4 Max, 64 GiB), loading real llama-server instances of several sizes, each with and without `--no-mmap`.

Signals, sampled at the interval recorded in each run:

- Host: `host_statistics64` (free, active, inactive, speculative, wired, purgeable, internal, external, compressor), `kern.memorystatus_level`, `kern.memorystatus_vm_pressure_level`, `vm.swapusage`.
- Per process: `ri_phys_footprint`, `ri_resident_size`, `ri_wired_size`, `ri_lifetime_max_phys_footprint`.
- Device: Metal `recommendedMaxWorkingSetSize`, `iogpu.wired_limit_mb`.
- llama.cpp's own buffer accounting, from its log (model, KV and compute buffers).

These are experiment thresholds, not product constants. Their numeric values live only in [000-criteria.json](000-criteria.json), which `hearth-probe report --criteria` reads; the table describes them. All must pass:

| id | criterion |
|---|---|
| a | Per-process footprint is within `footprint_vs_allocated_max_rel` of what llama.cpp says it allocated |
| b | The footprint change explains the host "used" change within max(`host_vs_footprint_rel`, `host_vs_footprint_abs_bytes`) |
| c | Available memory falls by ≥ `load_visibility_min_rel` of the footprint on load, and recovers ≥ `reclaim_min_rel` within the run's reclaim window |
| d | Idle noise (`noise_sigma_multiplier` × σ) is below the reserve floor (memsize − Metal working set) |
| e | Under induced pressure, a signal trips before swap grows or latency degrades |
| f | Free pages alone overstate use by more than `naive_overstatement_min_rel` (reproduces the old governor bug) |
| g | Predicted admission refuses every real overload and wrongly refuses no load that actually fit |

If the gate fails, admission falls back to learned per-model footprints with serialized loads, and this ADR records why.

## Observations before the gate

These come from passive sampling of a running llama-server on 2026-10-14:
- Metal `recommendedMaxWorkingSetSize` is 55,662,788,608 bytes (81% of RAM).
- `iogpu.wired_limit_mb` is 0.
- `ri_phys_footprint` reads 30.0 GB with a lifetime peak of 48.1 GB.
- `ri_wired_size` reads 0, so GPU memory is not visible as per-process wired.
- The machine was under memory pressure: level 2 (warn), swap 15.5 of 16.1 GB.
- `proc_pidpath` failed for that server because Homebrew had replaced its binary while it ran, so process matching also uses `proc_name`.

- 2026-10-15, idle (nothing loaded by the gate, a 27B llama-server serving a 262k context beside it): 180 samples at 500 ms over 90 s showed host available memory between 3.87 and 6.83 GiB, σ 0.82 GiB, so 3σ is 2.45 GiB. Step-to-step σ was 0.24 GiB. Pressure moved between normal and warn, swap was 19 of 20 GiB, and jetsam had killed a process that morning.
- What that means for the gate: a load is only visible above that noise if its footprint is well over 2.45 GiB, while only 4 to 7 GiB was available. A small model would fail the gate on noise and record nothing about the signals; a model big enough to clear the noise could push this machine into jetsam. The gate waits for a quieter machine (the 27B server stopped, swap drained) and a model of several GiB, as the decision above says.

## Fallback in place (2026-10-15)

The fallback named above ships for loads: hearthd starts one server at a time (`supervise.serialize_loads`, on by default). A server about to start waits while another it keeps is still loading, so two models never fill memory at the same moment. The start-time fit check (ADR-007) still refuses a model that can never fit the device. Learned per-model footprints are already recorded at each ready event (`footprint_at_ready`). Admitting from measured free memory waits for this gate.

## Results

Pending.
