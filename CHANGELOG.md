# Changelog

Entries are timestamped (UTC+08) with the commit time. Versions follow semver once v0.1.0 ships.

## Unreleased

### 2026-10-15 08:32 -hf projectors

- **`-hf` projectors.** `fit --port` and the `ports` oversized check count the multimodal projector that `llama-server -hf` downloads beside the model and loads.
- **Projectors kept off the GPU.** A projector held on the CPU (`--no-mmproj-offload`, `--mmproj-device none`) or not loaded (`--no-mmproj`) is not counted.

### 2026-10-15 08:20 projectors in fit

- `fit --mmproj FILE` counts a multimodal projector with the weights. `fit --port` and the `ports` oversized check read the server's own `--mmproj`.

### 2026-10-15 08:14 review fixes

- **`what-can-i-run`.** It sizes each model with its own cache type unless one is given.
- **`log --full`.** It marks unreadable entries with `kind: unreadable`.
- **DRM fdinfo.** An unknown size unit is reported as an error instead of being read as zero.

### 2026-10-15 08:09 log --full

- `hearth log --full` writes each snapshot in full as one JSON line, oldest first. `hearth log --all --since 1d --full > day.jsonl` archives a day of the journal.

### 2026-10-15 08:05 what-can-i-run workflow

- **New workflow.** `what-can-i-run CTX` is the eighth built-in workflow. It lists the models on disk that fit at that context and marks those already served. It then checks the best one with `fit` and says what would have to be unloaded first.
- **CI.** It runs clippy on the beta toolchain as an early warning.

### 2026-10-15 07:58 VRAM per runtime fixes

- **Which figure.** VRAM read from DRM fdinfo prefers resident device memory, when the kernel reports it.
- **Which descriptors.** Only descriptors open on `/dev/dri` are read.
- **Failures.** A source that cannot be read is reported. The other source still counts.
- **Reading the number.** The value is an upper bound per process. Do not sum it across ports or processes.

### 2026-10-15 07:53 exit cause metric

- hearthd's metrics add `hearth_server_exit_cause{server,cause}`: 1 for why each server last exited, when its log said.

### 2026-10-15 07:46 accelerator memory per process

- **VRAM per runtime.** On Linux, `ports` shows the VRAM each model runtime holds (`accelerator_bytes`) beside its host footprint: NVIDIA from `nvidia-smi`, AMD and Intel from DRM fdinfo.
- **Apple silicon.** Nothing is added there, because the footprint already counts Metal allocations.

### 2026-10-15 07:38 footprint at ready

- **Ready events.** Supervised servers' `ready` events record the server's memory footprint once it serves.
- **`fit`.** It mentions that footprint for the same model and context. It is recorded evidence only: the P0 gate has not yet shown how memory-mapped weights count in it.

### 2026-10-15 07:30 futile restart fixes

- **When restarts stop.** Only a start that never served can be judged futile. A missing model file is retried, because it may be on a volume that is not mounted yet. An allocation failed during a load counts as out of memory.
- **`hearth run`.** Its final error names the last event.

### 2026-10-15 07:23 futile restarts

- A supervised server that exits before serving because its model does not load or its arguments are refused is not restarted. The decision is `futile`, the restart budget is not charged, and you are notified. `hearth start` revives it once the command is fixed.

### 2026-10-15 07:19 exit cause and meanwhile fixes

- **Exit causes.** An exit's cause must come from the server's last lines: an old allocation warning no longer explains a later, unrelated exit. A failed allocation is reported as out of memory even when it made the load fail.
- **`diff --limit`.** It also bounds `meanwhile`.
- **`models --ctx`.** It keeps unreadable models in the list, with their error.

### 2026-10-15 07:11 exit causes

- **Exit causes.** When a server hearth supervises exits, the event says why, from llama.cpp's own log: `model_missing`, `load_failed`, `out_of_memory`, `port_in_use` or `bad_argument`, with the line that said it. `hearth run` prints it.
- **`server-died`.** The workflow reads the cause first. A missing model or a bad argument is not cured by a restart.

### 2026-10-15 07:08 models --ctx

- `hearth models --ctx N` lists only the models the device holds at N tokens of context, answering "what can I run here with 32k context".

### 2026-10-15 07:07 diff meanwhile

- `hearth diff` lists, under `meanwhile`, the notes and run events recorded between its two snapshots. Each diff then carries what was done next to what changed. `what-changed` reads it.

### 2026-10-15 07:03 models fixes

- **Paths.** `models --json` gives each file's path as found, not the cache blob it links to.
- **Folder walking.** Symlinked folders are not followed. An unreadable folder is reported as an error.
- **`--serving`.** If the server probe fails, the error is reported and the list is still printed.

### 2026-10-15 06:56 quantization folders

- `fit`, `models` and `org/repo:QUANT` names find GGUFs kept in a repo's quantization folders, such as `Q4_K_M/model-Q4_K_M-00001-of-00002.gguf`. Such a file is named `org/repo/Q4_K_M/file.gguf`.
- `models --json` gives each model's path.

### 2026-10-15 06:52 models --serving

- `hearth models --serving` marks the models a running server serves now, and where: llama.cpp by model file, Ollama by loaded model.
- `scripts/check.sh` runs CI's checks locally.

### 2026-10-15 06:47 models listing fixes

- **Store reads.** `models` reads each model from the store it is listed in. A stray file such as `.DS_Store` or one unreadable directory no longer hides other stores' models.
- **Errors.** Unfinished downloads are skipped. Names hearth cannot look up are reported.
- **`show --pointer`.** It refuses a pointer that does not start with `/`.

### 2026-10-15 06:39 hearth models

- **`hearth models [FILTER] [--cache-type T]`.** Lists every model in the Hugging Face cache, LM Studio and Ollama's store. Each line gives its architecture, weights, trained context and the largest context the device holds it at. Names are what `fit` takes. Only files are read.
- **MCP and workflows.** MCP serves `models`. `can-i-load` suggests models already on disk that would fit.

### 2026-10-15 06:36 show --pointer

- `hearth show --pointer /body/health/memory` prints one part of a snapshot, like `git show REF:PATH`. A wrong path names the deepest part that exists and what it holds. MCP's `show` takes `pointer`.

### 2026-10-15 06:30 compute buffer fixes

- **Ready events.** A `ready` event carries `compute_bytes` only when llama.cpp logged some. `fit` never treats zero as a measurement.
- **JSON.** `fit`'s JSON drops `excludes` when compute buffers were measured.
- **Fit note.** It says the value reflects that start's batch and flash-attention settings.

### 2026-10-15 06:22 learned compute buffers

- **Logged at start.** `hearth run` and hearthd now record the model, KV and compute buffer sizes that llama.cpp logs while loading, on each `ready` event.
- **Counted by fit.** `fit`, and so the `ports` oversized check, adds the compute buffers from the latest start of the same model at the same context. Its total is then measured rather than "excluding compute buffers". A start at a different context is only mentioned in a note.

### 2026-10-15 06:19 flash attention flag and headroom

- `fit` and the `ports` oversized finding write `-fa on`, the form current llama.cpp takes. The finding lists every value cache type that fits, not only the largest, which may leave no room for compute buffers.

### 2026-10-15 06:13 values-only cache types

- `fit` shows the cache types it costed. When the context does not fit, it also lists `fits_device_with_values`: the value cache types that alone make it fit, with keys left unchanged (llama.cpp `-ctv`, which needs `-fa on`). Keys carry more of attention's precision.
- The `ports` oversized finding names the largest such type.

### 2026-10-15 06:10 right-size-a-server workflow

- **New workflow.** The seventh built-in workflow, `right-size-a-server PORT`, turns `fit --port` into the flags to change: a smaller `-c`, or a smaller `-ctk`/`-ctv` that keeps the context. It then records that advice with `note`.
- **`can-i-load`.** It takes every model form `fit` takes, and leaves an MLX model's cache type to its dtype.

### 2026-10-15 06:05 log --all, and fixes for notes

- **`log --all`.** One timeline of every kind, newest first, like `git log --all`: snapshots, run events and notes, each with its kind and `~N`.
- **Bundle.** `bundle` withholds note text, because notes may quote keys.
- **Notes.** A note is capped at `state.max_note_bytes`. `note hello --json` no longer records `--json` as part of the message; words that look like flags go after `--`.
- **Thinning.** `state.thinned_kinds` refuses event kinds (`run`, `note`), which would otherwise be thinned away.
- **Tags.** A `tag --at` that fails leaves the existing tag as it was.
- **MCP.** Resources serve notes.

### 2026-10-15 06:00 notes

- **`hearth note [--by WHO] MESSAGE`.** Records what someone did or saw, such as "lowered -c to 131072 after the pressure", in the journal beside hearth's snapshots. `log --kind note` lists notes, and `--grep` searches them.
- **Readers.** The `what-changed` workflow reads notes. `bundle` includes the latest. MCP serves `note` and asks agents to write one after every change they make or advise.
- **Config.** The number of recent run events and notes in `bundle` is now `bundle.recent_events`, no longer a number in code.

### 2026-10-15 05:54 review fixes for grep, workflows and fit

- **log --grep.** Matches what a snapshot says, not its escaped JSON. Text matches as a substring with case ignored; a number matches only when equal, so pid 123 no longer matches 1234. Reading stops once `--limit` matches are found.
- **when-did-it-start.** Refers to snapshots by id, because new recordings shift `~N`. It compares matches across kinds by time. Run events go through `show` and `follow`.
- **Offload.** `ports` again flags a model wholly on the CPU when the machine has an accelerator, the case where nothing fit.
- **fit configs.** Gemma 2 and top-level Gemma 3 defaults are filled in. A zero `kv_lora_rank` means plain attention. A layer count that cannot be allocated is an error instead of an abort.

### 2026-10-15 05:51 tag --at

- `hearth tag NAME --at REF` names one earlier snapshot (an id, `~N`, `@AGE` or another tag, of `--kind`), like `git tag NAME COMMIT`, so a moment found with `log --grep` is kept past retention. MCP's `tag` takes `at` and `kind`.

### 2026-10-15 05:48 MLX cache type from the config

- `fit` on an MLX or safetensors model needs no `--cache-type`: it uses the config's `torch_dtype` (or `dtype`), which is what MLX caches in. This also covers `fit --port` on `mlx_lm.server`.

### 2026-10-15 05:46 when-did-it-start workflow

- A sixth built-in workflow (and MCP prompt), `when-did-it-start`, finds the first snapshot that mentions an error, finding code, model, port or pid, and diffs it against the one before.

### 2026-10-15 05:44 log --grep

- `hearth log --grep TEXT` lists only the snapshots whose recorded JSON mentions TEXT (case ignored), like `git log -S`. Each keeps its `~N`, and `--limit` counts matches. MCP's `log` takes `grep`.

### 2026-10-15 05:40 safetensors weights from the index

- `fit DIR` sums only the shards `model.safetensors.index.json` names, so a `consolidated.safetensors` copy is not counted twice. Without an index, `adapters.safetensors` is left out.

### 2026-10-15 05:38 MLX fit and offload review fixes

- **Latent attention.** `fit` on a DeepSeek or Kimi `config.json` costs the larger of the latent cache and MLX's expanded keys and values. Before, it costed far less than MLX holds.
- **Gemma defaults.** Gemma configs that leave `head_dim` and the head counts to Transformers defaults now fit, and their `head_dim` is 256, not `hidden_size / heads`.
- **Offload warnings.** `ports` no longer flags a model wholly on the CPU (a host without an accelerator, or `num_gpu 0`). Each model of one Ollama process is its own warning in `watch`.
- `fit --port` on `mlx_lm.server` fits the `--model` it was started with, not the first cached model it lists.

### 2026-10-15 05:24 MLX and safetensors models in fit

- `hearth fit DIR` fits an MLX or Transformers model directory (`config.json` plus `.safetensors`), with geometry read from the config.
- An `org/repo` name in the Hugging Face cache without a GGUF resolves to such a directory.

### 2026-10-15 05:20 partial offload in Ollama

- `hearth ports` warns when an Ollama model sits only partly on the accelerator (`size_vram` below `size`). `watch` reports it once.

### 2026-10-15 05:17 steadier ports warnings in watch

- `watch` and hearthd apply `watch.confirm_rounds` to `ports` warnings too, so a transient probe failure no longer makes them flap.

### 2026-10-15 05:14 snapshots as MCP resources

- `hearth mcp` serves recorded snapshots as resources at `hearth://KIND/REFERENCE`, for example `hearth://doctor/@1h` or `hearth://ports/healthy`, with a list and a URI template.
- Argument parsing keeps empty arguments: all of them on Linux, and every one but a first-position one on macOS.

### 2026-10-15 05:07 cache types and oversized-check fixes

- **Cache types from the command line.** The last `-ctk`/`-ctv` wins. An absent flag means the type is unknown, not f16, because the server may take it from its environment.
- **Oversized check.** `ports` checks each llama.cpp server with what it already probed and that process's own arguments, without probing again. Its message handles weights that alone exceed the device.
- **Fit check before start.** A too-big verdict is re-checked at each start.

### 2026-10-15 04:55 servers configured past the device are flagged

- `hearth ports` warns about any llama.cpp server whose model, at its context and cache types, exceeds the device. The warning gives the largest context that fits and the cache types that would keep the context.
- `watch` and hearthd report it once.
- `ports` findings carry `code` and `pid`.

### 2026-10-15 04:52 fit a running server with no arguments

- `hearth fit --port P` reads a llama-server's cache types from its command line, with f16 when absent. `--cache-type` is optional with `--port`.
- New `hearth_sense::process_args`.
- A too-big refusal from hearthd or `hearth run` names the cache types that would keep the context.

### 2026-10-15 04:45 fit suggests cache types

- When a context does not fit the device, `hearth fit` lists the KV cache types it would fit with, each with its total (`fits_device_with`).

### 2026-10-15 04:42 fit check refinements

- The fit check before start refuses only a too-big verdict. A check that cannot run is reported, and the start goes ahead. Verdicts are reused across restarts.
- `fit --port` counts llama.cpp's per-sequence `n_ctx` times its slots.
- `ctx = 0`, an empty `model` and unknown cache types are rejected up front.

### 2026-10-15 04:33 hearth run fit check

- `hearth run --model M --ctx N --cache-type T -- …` checks before every start that the model fits the device, as hearthd does for declared servers.

### 2026-10-15 04:30 fit check before start

- `[servers.NAME]` can declare `model`, `ctx` and `cache_type`, together. hearthd checks before every start that the model at that context fits the device, and refuses one that cannot. The refusal reports the sizes and the largest context that fits.
- `ports` shows each loaded Ollama model's context.

### 2026-10-15 04:24 fit the server as it runs

- `hearth fit --port P --cache-type T` with no `--ctx` uses the context the server reports: llama.cpp's `n_ctx`, or Ollama's `context_length` from `/api/ps`, now recorded by `ports`.

### 2026-10-15 04:18 review fixes: GPUs, cgroups, PSI, model stores

- `nvidia-smi` is bounded by the new `accelerators.query_timeout_ms`. Its failures are reported.
- GPU metrics are indexed, and AMD APUs count as unified memory.
- **cgroups.** The cgroup total is the tightest limit, and PSI is read from the binding cgroup.
- **fit.** `fit` checks a discrete GPU's free memory. Store lookups are stricter.
- **Configuration.** The `control.*` knobs are validated again.
- **docs.** New `docs/configuration.md`, generated by `hearth config --markdown` and checked by CI.

### 2026-10-15 04:08 discrete GPUs on Linux

- On Linux, AMD GPUs are read from amdgpu sysfs and NVIDIA GPUs from `nvidia-smi`.
- `doctor` shows each GPU's memory, the metrics export `hearth_gpu_memory_total_bytes` and `hearth_gpu_memory_used_bytes`, and `fit` checks against the GPU's memory.

### 2026-10-15 04:03 cgroup-aware memory on Linux

- Inside a memory-limited cgroup, such as a container or a systemd slice, total and available memory are the smaller of the host's and the tightest limit over the process: its own cgroup or any ancestor's. Available memory counts the cgroup's reclaimable page cache. `doctor --explain` says which limit applied.

### 2026-10-15 03:58 Linux memory pressure

- On Linux, the memory pressure level is derived from PSI using the new `[pressure]` keys `psi_some_warn` and `psi_full_critical`. `doctor`, `watch`, `fit` and the metrics use it, and `doctor --explain` shows the derivation. Before this, Linux never warned.

### 2026-10-15 03:54 review fixes: workflows, journal reads, config checks

- **Workflows.** Arguments are substituted in one pass. Undeclared or mistyped arguments are refused. Argument lists, duplicate names and backticks are validated. Problems are reported on MCP's stderr and in its instructions.
- **Journal.** `log --since` no longer reports snapshots thinned away mid-read. `log --stats` reports unreadable tags.
- **Configuration.** Unknown `state.thinned_kinds` and NUL in `servers.*.env` values are rejected.

### 2026-10-15 03:47 fit by Hugging Face and LM Studio name

- `hearth fit org/repo:Q6_K` finds the model in the Hugging Face cache or LM Studio's models directory. It also accepts `org/repo/file.gguf`.
- `hearth fit --port` works for LM Studio servers.

### 2026-10-15 03:43 log --stats

- `hearth log --stats` (MCP `log` with `stats`) reports, per kind, how many snapshots are kept, the time they span and their size on disk, plus the number of tags.

### 2026-10-15 03:41 retention by age

- New `state.tiers` thins ports and doctor snapshots as they age. By default it keeps all of them for an hour, one per 10 minutes for a day, and one per hour for a week. `state.thinned_kinds` lists the kinds this applies to; `run` events are never thinned.
- `state.max_snapshots_per_kind` is now a hard cap of 1000.
- `hearth config` documents array settings such as `state.tiers`.

### 2026-10-15 03:36 your own workflows

- `workflows.json` beside the user's config adds workflows to the built-in ones, or replaces them by name. MCP serves them as prompts. A workflow that names a tool hearth does not have is skipped with the reason.
- `hearth workflows` lists every workflow with its source. `hearth workflows NAME --arg KEY=VALUE` prints one, filled in.
- `doctor --explain` names each platform's own source (sysctl or `/proc` file), and MCP `doctor` takes `explain`.

### 2026-10-15 03:04 doctor --explain

- `hearth doctor --explain` (MCP `doctor` with `explain`) lists, for every memory figure and limit, the counter, sysctl or file it comes from and the formula that combines them.

### 2026-10-15 02:59 server env and dir

- `[servers.NAME]` takes `env = { VAR = "value" }` and `dir = "/abs/path"`. Both are validated, and `env` values are redacted from bundles.

### 2026-10-15 02:54 time references

- A snapshot reference can be `@AGE` (`@30m`, `@2h`, `@1d`): the latest snapshot at least that old, accepted wherever an id, `~N` or a tag is.
- `hearth log --since AGE` lists only recent snapshots. MCP `log` takes `since`.

### 2026-10-15 02:46 release workflow

- `.github/workflows/release.yml` builds `hearth` and `hearthd` for aarch64-apple-darwin and for x86_64 and aarch64 Linux (static musl). Each archive is packaged with its SHA-256.
- A pushed `v*` tag publishes a GitHub release. Run by hand, the workflow only builds. Its first run built all three platforms.

### 2026-10-15 02:44 review fixes: reload, server pid, metrics

- **reload.** Only one reload runs at a time. Leaving servers stop in parallel. Orders during a reload are refused with "try again", not dropped.
- **server_pid.** `server_pid` comes from the tether's process group, so a process someone else runs on the port is never taken for the server. It is cleared when the server goes. `hearth_sense::process_group` is new.
- **Metrics.** A stalled scraper no longer blocks other scrapes: each request has a total deadline and its own handler.

### 2026-10-15 02:38 owned and observed ports

- `hearth ports` (and MCP `ports`) reports `owned_by` for each listener: the hearthd server it is, or null for processes hearth only observes.

### 2026-10-15 02:35 hearth reload

- `hearth reload` (MCP `reload`) applies the configuration's `[servers]` to a running hearthd. New servers start, removed ones stop, and changed ones restart with their new definition. Every other server keeps its process.

### 2026-10-15 02:25 status names the real server process

- `hearth status` showed the tether's pid. It now reports `server_pid`, the process listening on the server's port, with its `footprint_bytes`.
- Metric `hearth_server_footprint_bytes{server}`.

### 2026-10-15 02:23 Prometheus metrics

- With `[metrics] listen = "127.0.0.1:9469"` (new, off by default), hearthd serves `GET /metrics`. It reports each server's state and restarts, and the machine's memory, swap, pressure, open files and threads.

### 2026-10-15 02:19 shell completions

- `hearth completions SHELL` prints a completion script for bash, elvish, fish, powershell or zsh, generated from the command definitions.
- Two follow-up fixes:
  - `hearth bundle` scrubs configuration error text from every part, including `doctor`'s state errors.
  - The review fixes above passed CI.

### 2026-10-15 02:14 review fixes: bundle, MCP arguments, orders, tags, follow, logs

- **bundle.** It no longer includes configuration error text, which quoted the offending line. A webhook token leaked through it.
- **MCP arguments.** They are checked against the tool's schema (types, enums). Values starting with `-` stay values.
- **hearthd orders.** The latest order wins. `start` and `restart` begin a fresh crash-loop budget.
- **logs.** Output is bounded per line by `supervise.output_line_bytes` (new key), and answers are trimmed to fit `control.max_message_bytes`.
- **Tags.** A corrupt tag no longer stops retention, and a tag never names a pruned snapshot.
- **follow.** `follow --kind run` lists every event.
- **Write errors.** Write errors other than a closed pipe exit 1.

### 2026-10-15 01:54 hearth bundle

- `hearth bundle [--since T]` (MCP `bundle`) gathers doctor, ports, incidents, hearthd status, recent `run` events, tags and settings into one JSON document. A part that fails is included as its error.
- The webhook URL and server arguments are redacted. Process names and paths stay, so read the bundle before sharing it.

### 2026-10-15 01:52 hearth tag

- `hearth tag NAME` names the latest snapshot of each kind. Tagged snapshots are never pruned, and the name works wherever a snapshot is asked for: `hearth diff --from healthy`, `hearth show healthy`.
- `hearth tag` lists tags; `--delete` removes one.
- MCP tool `tag`. The `what-changed` workflow compares against a tag.

### 2026-10-15 01:48 hearth stop, start and logs

- `hearth stop NAME` stops a hearthd server and keeps it down, freeing its memory, until `hearth start NAME`. `start` also revives a server hearthd gave up on.
- `hearth logs NAME`: the server's last `supervise.output_lines` (new key) lines of output, across restarts.
- MCP tools `stop`, `start` and `logs`. The workflows `server-died` and `can-i-load` use them.

### 2026-10-15 01:43 hearth show and follow

- `hearth show [--kind K] [REF]`: one recorded snapshot in full.
- `hearth follow PATTERN [--kind ports|doctor|run] [--all]`: one process, port or server through every snapshot, listing when it appeared, changed and went away. A leak shows as steady growth.
- MCP tools `show` and `follow`, and the workflow prompt `find-a-leak`.
- MCP tool calls are translated from each tool's definition; unknown arguments are refused.
- `hearth` exits quietly when the reader of its output goes away (`| head`) instead of panicking.

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
