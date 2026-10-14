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

On Linux, `hearth incidents` reads the kernel log with `journalctl _TRANSPORT=kernel -o short-iso`, which covers every boot the journal kept. `journalctl -k` would imply the current boot only, and the kill that matters after a reboot is in the boot before. A journal kept in memory only (`Storage=volatile`) keeps nothing across reboots, whatever is asked. It reports two kinds of incident:
- **OOM kills.** Machine-wide or by a memory cgroup's limit. Each gives the process, its pid, and the anonymous memory the kill freed.
- **Hung tasks.** A task in uninterruptible sleep past the kernel's hung-task timeout, which is what a process stuck on a dead device or GPU driver looks like.

Lines are matched on the kernel's own format strings, from `mm/oom_kill.c` and `kernel/hung_task.c`.

Reading the kernel log usually needs the `systemd-journal` or `adm` group. When it is refused, the command says so and why, rather than reporting no incidents.

## Addendum (2026-10-15): Ollama model names in `fit`

`hearth fit` also accepts an Ollama model name (`qwen3:8b`, `user/model:tag`, `host/ns/model:tag`), split the way Ollama splits it.
- **Lookup.** The weights are found in Ollama's on-disk store, not by asking Ollama: the manifest for that tag, then its layer of media type `application/vnd.ollama.image.model`, then `blobs/sha256-<hex>`. Ollama need not be running.
- **Stores.** The store is `OLLAMA_MODELS` when set. Otherwise it is Ollama's default for a user install (`~/.ollama/models`) or for the Linux system service (`/usr/share/ollama/.ollama/models`).
- **Paths win.** A path that exists is always taken as a path. Anything starting with `.`, `/` or `~`, or containing `..`, is never taken as a model name.

## Addendum (2026-10-15): MCP prompts

`hearth mcp` declares the `prompts` capability and serves diagnostic workflows from `mcp-prompts.json`:
- `diagnose-slow-machine`;
- `server-died`;
- `can-i-load`;
- `what-changed`.

Each workflow names the tools to call in order, how to read their answers, and the shape of the reply. Each also states what not to do: never load a model, and restart only servers hearthd owns. Arguments have documented defaults, and a missing required argument is a JSON-RPC invalid-params error.

A test fails when a workflow names a tool that does not exist. The workflows therefore cannot drift from the tool list.

## Addendum (2026-10-15): `fit --port`

`hearth fit --port P` asks the server on `P`, with the same read-only fingerprint as `ports`, which model it serves, and fits that file at the context given.
- **llama.cpp** reports its model file.
- **Ollama** reports the models it has loaded. One loaded model is used directly; with several, `--model` picks which. The weights are then found in Ollama's store.

This answers "could my running server take a different context" without restarting it.

On the development machine, the running llama-server (`--ctx-size 262144`, q8_0 K and V) came out at:
- 34.53 GiB of KV cache, against a measured footprint of about 35 GiB (macOS footprints exclude memory-mapped weights);
- plus 22.38 GiB of weights, 56.9 GiB in total.

That is above Metal's recommended working set of 51.84 GiB, the memory pressure `doctor` and `watch` kept reporting. The largest context that fits the device is 223,620 tokens.

## Addendum (2026-10-15): Hugging Face and LM Studio names in `fit`

`fit` also takes `org/repo`, `org/repo:QUANT` (as `llama-server -hf` takes it) or `org/repo/FILE`. It looks the name up in the Hugging Face cache (`HF_HUB_CACHE`, else `HF_HOME/hub`, else `~/.cache/huggingface/hub`), using the snapshot `refs/main` names, then in LM Studio's models directory.
- **Candidates.** Multimodal projectors and later shards of split models are never candidates.
- **Ambiguity.** Several matches are listed, with how to pick one.
- **Order.** These stores are searched before Ollama's, so an `org/repo:tag` name that one of them has is not mistaken for an Ollama namespace.
- **LM Studio servers.** `fit --port` on an LM Studio server fits its loaded model the same way.

## Addendum (2026-10-15): memory pressure on Linux

Linux publishes no memory pressure level. It publishes how much of the last ten seconds tasks stalled waiting for memory (PSI, `/proc/pressure/memory`). hearth derives a level from that with two policy knobs:
- **warn:** `pressure.psi_some_warn`, a percentage of time some task stalled;
- **critical:** `pressure.psi_full_critical`, a percentage of time all tasks stalled at once.

For comparison, systemd-oomd starts killing at 60. `doctor --explain` shows the PSI figures against the thresholds.

A level the kernel declares (macOS) is always used as is. `doctor`, `watch`, `fit` and the metrics all use the same level. Findings no longer say "the kernel reports" for a level hearth derived.

## Addendum (2026-10-15): discrete GPUs on Linux

On Linux, `hearth-sense::accelerators` reports GPUs the way their drivers tell an unprivileged user:
- **AMD:** amdgpu's `mem_info_vram_total` and `mem_info_vram_used` under `/sys/class/drm/cardN/device`, named by `product_name`.
- **NVIDIA:** `nvidia-smi --query-gpu=name,memory.total,memory.used`. hearth runs NVIDIA's own tool rather than linking NVML, which keeps FFI out.

How they are used:
- `doctor` prints a line per discrete GPU.
- The metrics export `hearth_gpu_memory_total_bytes` and `hearth_gpu_memory_used_bytes`.
- `fit` takes a discrete GPU's memory as the device ceiling. With several GPUs it uses the smallest, the one a model must fit to go on any single device; splitting a model across devices is not modelled.

Tests read a fixture sysfs tree and recorded `nvidia-smi` output. CI has no GPU.

## Addendum (2026-10-15): review corrections

- **GPU queries.** `nvidia-smi` is killed after `accelerators.query_timeout_ms`, because a wedged driver leaves it stuck in the kernel. "No nvidia-smi" means no NVIDIA GPU; any other failure is reported, not hidden. `doctor` prints it, and the metrics page carries it as a comment line.
- **AMD APUs.** When GTT is larger than the VRAM carve-out, the GPU is treated as unified memory with GTT as its size, so `fit` does not take the firmware's small carve-out as the device ceiling.
- **Metric labels.** GPU series carry an `index` label, so two identical cards are two series.
- **cgroups.** The total is the smallest limit over every level. Availability is the least room left at any level.
- **PSI.** It is read from the binding cgroup's `memory.pressure` when there is a limit. `doctor --explain` names the file.
- **Discrete GPU fits.** "Fits right now" is checked against the device's free memory (total - used) when the driver reports usage.
- **Model stores.** `fit --port` on LM Studio looks only in LM Studio's directory. In both stores, projectors (`*mmproj*`) and later shards (`-NNNNN-of-NNNNN`, index above 1) are not candidates. A Hugging Face revision must be a plain hash. Several snapshots without `refs/main` is an error, not "not found". `HUGGINGFACE_HUB_CACHE` and `XDG_CACHE_HOME` are honoured.
- **Configuration errors.** A configuration that does not load is an error for the commands that derive a pressure level, not a silent "no level".

## Addendum (2026-10-15): fitting a running server as it runs

`hearth fit --port P` needs nothing else for a llama.cpp server:
- **Context.** It is `/props` `n_ctx` times `total_slots`, since `n_ctx` is per sequence.
- **Cache types.** They are read from the server's own command line (`-ctk`/`--cache-type-k`, `-ctv`/`--cache-type-v`). The last occurrence wins, as in llama.cpp. When a flag is absent the type is unknown and `--cache-type` is needed: the server may take it from `LLAMA_ARG_CACHE_TYPE_*` in its environment, which is not read. Known limits, left for later: `ports` re-runs the fit per server and probes loopback only; a transient failure of that fit drops the finding for a round. `hearth_sense::process_args` reads it: `KERN_PROCARGS2` on macOS, `/proc/PID/cmdline` on Linux. The kernel only lets this user read their own processes' arguments.

When the context does not fit, `fits_device_with` names the cache types it would fit with. hearthd's refusal message suggests them too.

