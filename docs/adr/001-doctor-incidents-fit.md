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
- `what-changed`;
- `find-a-leak`;
- `when-did-it-start`, which uses `log --grep` to find the first snapshot that mentions something and diffs it against the one before.
- `what-can-i-run`, which lists the models on disk that fit at a given context (`models --ctx --serving`), checks the best one with `fit`, and says what would have to be unloaded first;
- `right-size-a-server`, which turns `fit --port` into the flags to change (a smaller `-c`, or a smaller `-ctk`/`-ctv` that keeps the context) and records the advice with `note`.

Each workflow names the tools to call in order, how to read their answers, and the shape of the reply. Each also states what not to do: never load a model, and restart only servers hearthd owns. Arguments have documented defaults, and a missing required argument is a JSON-RPC invalid-params error.

A test fails when a workflow names a tool that does not exist. The workflows therefore cannot drift from the tool list.

## Addendum (2026-10-15): `fit --port`

`hearth fit --port P` asks the server on `P`, with the same read-only fingerprint as `ports`, which model it serves, and fits that file at the context given.

For a llama.cpp server, "fits right now" counts the memory the server already holds as available: a restart at another size frees it first. This applies on unified memory only. An Ollama daemon is not counted, because its model lives in a runner child.
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
- **Cache types.** They are read from the server's own command line (`-ctk`/`--cache-type-k`, `-ctv`/`--cache-type-v`). The last occurrence wins, as in llama.cpp. When a flag is absent the type is unknown and `--cache-type` is needed: the server may take it from `LLAMA_ARG_CACHE_TYPE_*` in its environment, which is not read. `ports` fits each llama.cpp server from what it has already learned: the model and context from the answer it probed, and the cache types from that exact process's command line. It probes nothing again, and reads no other process. A server whose fit cannot be computed is left without a finding for that round. `hearth_sense::process_args` reads it: `KERN_PROCARGS2` on macOS, `/proc/PID/cmdline` on Linux. The kernel only lets this user read their own processes' arguments.

When the context does not fit, `fits_device_with` names the cache types it would fit with. hearthd's refusal message suggests them too.

## Addendum (2026-10-15): MLX and safetensors models

`fit` also takes a model directory with `config.json` and `.safetensors` files, which is how MLX (`mlx_lm.server`) and Transformers models are stored. The Hugging Face cache lookup returns such a repo as a directory. The weights are the safetensors files' total size; for MLX they are already quantized.

The geometry comes from the config's Transformers keys:
- `num_hidden_layers`;
- `num_key_value_heads`, defaulting to the attention heads; 0 marks a recurrent layer;
- `head_dim`, defaulting to `hidden_size / heads` (medium confidence, with a note);
- `sliding_window` with per-layer `layer_types`, or a `sliding_window_pattern` period; linear or mamba layers are recurrent; an unknown layer type is costed as full attention with a note;
- `max_position_embeddings` as the trained context.

A multimodal config's `text_config` is used. A key missing there is read from the top level, then from the Transformers defaults of that model type. Gemma configs rely on those defaults: their `head_dim` is 256, not `hidden_size / heads`, so deriving it would undercount.

Latent attention (`kv_lora_rank`, as in DeepSeek and Kimi) differs from GGUF. llama.cpp caches the compressed latent (`kv_lora_rank + qk_rope_head_dim` per token). MLX caches expanded keys and values (`heads × (qk_nope_head_dim + qk_rope_head_dim)` and `heads × v_head_dim`), which for DeepSeek-V3 is about 2.9 times as much. `fit` costs the larger, at medium confidence, so it never says a model fits when a runtime would not hold it.

A sliding window without `layer_types` or a pattern is costed as full attention and marked medium confidence.

On `--port` with an OpenAI-compatible server, the `--model` the server was started with wins over its `/v1/models` listing, because `mlx_lm.server` lists the models in the cache, not the one it serves.

## Addendum (2026-10-15): compute buffers learned from starts

A fit used to leave out the runtime's compute buffers, because no file states them. llama.cpp logs them as it loads ("llama_context: Metal compute buffer size = N MiB"), next to its model and KV buffers. The supervisor sums those lines for each start, and the `ready` event carries them in the run journal.

`fit` looks in the run journal for the latest ready event of the same model file at the same total context (`n_ctx` × slots). When it finds one, it adds those compute bytes to the total, the device check, the largest context and the cache-type alternatives, and names the event in a note. A start at another context is reported in a note but not counted, because compute buffers depend on the batch and context settings. A measured value replaces the estimate, so `excludes` disappears from the report.

Only the compute lines are read, and only a start that logged a non-zero amount counts as measured. Lines still unread when the port first answers are not counted; llama.cpp logs them before it serves, so this is rare and errs low. Finding the event reads run events newest first until one matches, so a fit costs up to `state.max_snapshots_per_kind` small reads when none does; `ports` runs one fit per llama.cpp server each round. Run events are few (one per start, exit or wedge), so this is accepted as a known cost rather than indexed. The value is that start's: compute buffers also depend on the batch sizes and flash attention, and without flash attention they grow with the context, which the note says.

## Addendum (2026-10-15): `hearth models`

`hearth models` lists every model the stores that `fit` reads hold: Hugging Face cache repos (each GGUF weights file, or the repo when it is a safetensors model), LM Studio's directories, and Ollama's manifests. Each line gives a name `fit` accepts, the store, the architecture, the weights, the trained context, and the largest context the device holds it at. That context is computed by the same geometry and device ceiling as `fit`, with `--cache-type`, or the model's own dtype for MLX. A GGUF without a cache type is listed unsized, because llama.cpp's f16 default is a runtime's choice and not the model's. A store that is absent lists nothing; one that cannot be read is reported, never skipped silently. Only files are read.

## Addendum (2026-10-15): footprint at ready

Each `ready` event that `hearth run` and hearthd record now carries `footprint_at_ready`: the footprint, resident and peak memory of the server's listening process once it serves. On macOS this is `phys_footprint`, and on Linux it is `Pss`. `fit` reports it in the same note as the learned compute buffers, but does not use it in its verdict. Whether a memory-mapped model's weights count in that number is exactly what the P0 gate (ADR-000) measures, so until the gate runs this is recorded evidence, not an input.

## Addendum (2026-10-15): projectors and draft models

A llama-server can hold more than its model on the accelerator.

- **Multimodal projector.** `fit` counts the projector's weights. It is `--mmproj` as given, else the server's own `--mmproj`/`-mm`, else the projector `-hf` downloads beside the model (when that folder holds exactly one). A projector kept on the CPU (`--no-mmproj-offload`, `--mmproj-device none`) or not loaded (`--no-mmproj`) is not counted. Only its weights are counted, not the compute it uses to encode images.
- **Draft model.** For speculative decoding, `fit` counts the draft's weights and its own KV cache. That is `--draft` with `--draft-ctx` and `--draft-cache-type`, or the server's `-md`, `-cd` and `-ctkd`/`-ctvd`. Its context defaults to the main one, and its cache type defaults to f16, as in llama.cpp.
- **What reads these.** The `ports` oversized check passes both on. On the development machine, the user's `-hf` server loads an 888 MiB projector, which lowers its largest context from 223,620 to 217,037 tokens.
- **Code layout.** The command-line readers live in `server_args.rs`, model files in `model_files.rs`, and journal measurements in `measured.rs`, so `fit.rs` stays one concern.

