# ADR-001: Box doctor, crash forensics and preflight fit

- Status: **Accepted**
- Date: 2026-10-14

## Context

The failures that cost the most time on a machine running local models are not always inside a model server:
- A stream proxy leaked file descriptors until `kern.maxfiles`.
- A machine with thousands of threads stopped scheduling and the watchdog rebooted it.
- A GPU memory driver panicked.
- A server was started with a context that could never fit.

Each was diagnosed by hand, by people and by AI agents, from scattered sources. These checks need no daemon, so they ship before the gateway: they are useful on their own, and the daemon will reuse them.

## Decision

Three `hearth` subcommands. Each prints a human summary or, with `--json`, a stable document for scripts and agents.

### `hearth doctor`

Reports memory (available, kernel pressure level, swap), open files, threads and processes against the kernel's own limits. It names:
- the fewest processes that hold half of all open files, and half of all threads;
- the process closest to its per-process file limit.

**No thresholds are invented.** A finding is either:
- something the kernel declares (pressure warn or critical); or
- a projection from growth measured over `--watch SECONDS`, stated as time to the kernel's limit.

Every finding carries a `remediation`.

Sources:
- macOS: `PROC_PIDLISTFDS`, `PROC_PIDTASKINFO`, `processor_set_statistics(PROCESSOR_SET_LOAD_INFO)` for the system thread total (the figure `top` shows, including processes owned by other users), `kern.num_files`, `kern.maxfiles`, `kern.maxfilesperproc`, `kern.num_threads`, `kern.num_taskthreads`, `kern.maxproc`.
- Linux: `/proc/loadavg` (system thread total), `/proc/sys/fs/file-nr`, `nr_open`, `threads-max`, `pid_max`, `/proc/*/fd`, `Threads:`.

Processes owned by other users are counted as not inspectable rather than guessed.

### `hearth incidents`

Classifies the reports macOS writes to `DiagnosticReports`, using only text the kernel puts in them:
- **Kernel panic:** watchdog, GPU memory driver, or other.
- **Jetsam:** the largest process, and the processes killed with their reasons.
- **Resource limit:** disk writes, CPU or wakeups, with the action taken.
- **Shutdown stall.**

Resource-limit reports are counted by default and listed with `--all`.

The first run on the development machine found a GPU-driver panic: `completeMemory() prepare count underflow @IOGPUMemory.cpp:550`, on 2026-09-22. It is a third failure class besides watchdog and OOM.

Linux reports (kernel OOM-killer lines) are not read yet. The command says so instead of printing an empty list.

### `hearth fit`

Inputs are a GGUF model, a total KV context and cache types. It reports weights plus KV bytes, computed from the model's own metadata:
- per-layer geometry: full, sliding-window, latent and recurrent;
- spec defaults for missing keys;
- a confidence level and notes for every assumption.

It compares that total against:
- the device ceiling: the Metal recommended working set, or `iogpu.wired_limit_mb` when it is set lower;
- available memory and the kernel pressure level.

From those it reports whether the model fits the device and fits right now, plus the maximum context each allows, capped at the trained context.

Runtime compute buffers are excluded, and the output says so. They are learned after a model's first load, which needs the daemon.

"Fits right now" is marked `fits_now_provisional: true`. llama.cpp memory-maps weights, and before they are wired the pages may count as reclaimable file cache. Whether available memory really drops by the full weights is criterion (c) of the P0 gate, which has not run yet.

### GGUF reader

The GGUF reader is bounded:
- every length is checked against the bytes left in the file before anything is allocated;
- only arrays in the model's `{arch}.` namespace are kept, so memory scales with layer count and not with whatever a file claims.

## Consequences

- An agent can answer "what is wrong with this machine", "what crashed it" and "will this model fit" in one call each, without root.
- These checks share their sensing code with the admission controller, so the daemon's decisions and the CLI's answers cannot disagree.
- Still to do:
  - CLOSE_WAIT socket counts per process need socket-level `proc_pidfdinfo`, which `libc` does not expose. Open-file growth under `--watch` already names a leaking process.
  - Linux crash forensics.

## Addendum (2026-10-15): Linux incidents

On Linux, `hearth incidents` reads the kernel log through `journalctl -k -o short-iso`. It reports two kinds of incident:
- **OOM kills.** Machine-wide or by a memory cgroup's limit. Each gives the process, its pid, and the anonymous memory the kill freed.
- **Hung tasks.** A task in uninterruptible sleep past the kernel's hung-task timeout, which is what a process stuck on a dead device or GPU driver looks like.

Lines are matched on the kernel's own format strings, from `mm/oom_kill.c` and `kernel/hung_task.c`.

Reading the kernel log usually needs the `systemd-journal` or `adm` group. When it is refused, the command says so and why, rather than reporting no incidents.

