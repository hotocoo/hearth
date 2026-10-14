# hearth

**A control plane for local inference.** One daemon in front of every model runtime on a machine (llama.cpp, MLX, Ollama, LM Studio, ComfyUI, anything OpenAI-compatible), so that many apps can share one box without it OOMing, wedging or panicking.

> Status: **P1 in progress.** Shipped: `hearth-probe` (measurement instrument) and the `hearth` CLI (`doctor`, `incidents`, `fit`). The daemon is not built yet; nothing below the "Planned" line has shipped. Each phase lands implemented, tested on real runtimes, and documented ([docs/adr](docs/adr)), or it does not land.

## Why

Run several local models for several apps on one Mac or Linux box and the same failures keep coming back:

- **Memory nobody admits.**
  - Every runtime budgets only itself: Ollama's scheduler, LM Studio, llama.cpp router mode, oMLX.
  - Two of them load 40 GB between them, the kernel jetsam-kills something, or the watchdog panics the machine.
  - Model-swappers such as [llama-swap](https://github.com/mostlygeek/llama-swap) work from static rules. Measured-memory scheduling is [an open request](https://github.com/mostlygeek/llama-swap/issues/1004) there.
- **Backends that die quietly.**
  - After one Metal command-buffer OOM, llama.cpp marks the backend as failed. Every later request then fails within milliseconds until the process is restarted ([#28166](https://github.com/ggml-org/llama.cpp/issues/28166), [#29521](https://github.com/ggml-org/llama.cpp/issues/29521)).
  - Ollama's scheduler can hang forever.
  - Clients just see 500s or timeouts.
- **Context that balloons.**
  - An unset `num_ctx` or an over-large `-c` reserves tens of GB of KV cache.
  - Sliding-window and hybrid models make the arithmetic non-obvious.
- **Plumbing every app re-implements.**
  - Each app ends up with its own memory governor, poison detection, restart-and-retry, readiness probe and timeout chain, and each copy is subtly wrong.
  - One example is a stream proxy that leaked two file descriptors per disconnected client until the box ran out.

None of the existing tools combine these:
- measured-memory admission across heterogeneous runtimes;
- detection of poisoned or wedged backends with transparent restart and retry;
- per-model KV budgeting;
- fair scheduling across client apps.

hearth does.

## Measured so far

Run `hearth-probe` on an Apple M4 Max (64 GiB) with no elevated privileges:
- Metal `recommendedMaxWorkingSetSize` is 55.66 GB (81% of RAM).
- `iogpu.wired_limit_mb` is 0, so the kernel default applies.
- `proc_pid_rusage` `ri_phys_footprint` includes a llama-server's Metal allocations: 30.0 GB footprint, peak 48.1 GB.
- `ri_wired_size` reads 0 for the same process, so per-process "wired" is not where GPU memory shows up.
- `free_count` alone made a 64 GB machine look permanently critical. hearth uses `total − (app + wired + compressed)`, Activity Monitor's definition.
- A package upgrade under a running server makes `proc_pidpath` fail, so processes are matched by kernel name as well as by path.

The P0 gate checks, on real llama-server loads, that these signals predict what the kernel actually does: [scripts/p0-gate.sh](scripts/p0-gate.sh) records and `hearth-probe report` evaluates.

## hearth CLI

```sh
cargo build --release -p hearth
./target/release/hearth doctor              # memory, open files, threads vs kernel limits; who holds them
./target/release/hearth doctor --watch 30   # project time to each limit from measured growth
./target/release/hearth incidents           # kernel panics (watchdog / GPU driver / other), jetsam, stalls
./target/release/hearth fit model.gguf --ctx 32768 --cache-type q8_0   # fits the device? fits now? max context?
```

Every command takes `--json`. Every finding carries a `remediation`.

For AI agents, `hearth mcp` serves the same commands as MCP tools over stdio: `doctor`, `incidents`, `fit` and `process_memory`. For Claude Code:

```sh
claude mcp add hearth -- /path/to/hearth mcp
``` On the development machine, the first `hearth incidents` run found a kernel panic inside the GPU memory driver (`IOGPUMemory.cpp:550`), which is neither a watchdog reboot nor an OOM.

## hearth-probe

```sh
cargo build --release -p hearth-sense --bin hearth-probe
# host memory + every llama-server, as JSON lines
./target/release/hearth-probe sample --interval-ms 100 --match llama-server --out run.jsonl
# full gate: baseline, load A, load B, fill context, unload, reclaim.
# Every experiment parameter is explicit; the script has no built-in values.
INTERVAL_MS=100 CTX=8192 BASELINE_S=60 SETTLE_S=20 RECLAIM_S=30 FILL_TOKENS=4096 \
  scripts/p0-gate.sh runs/$(date +%s) small.gguf larger.gguf
./target/release/hearth-probe report runs/<id>/samples.jsonl --criteria docs/adr/000-criteria.json
```

## Planned

All of this is design, not shipped code. The design is in the ADRs as each phase lands.

- **Admission from measurement.**
  - Each memory pool gets a budget from the device ceiling, the measured resident footprints and a measured reserve.
  - Load cost comes from GGUF geometry and is corrected by the footprint learned per model.
  - If measurement fails, hearth serializes. It never fails open.
- **Recovery.**
  - A classifier sorts failures as transient, capacity, poisoned, wedged or model-missing. It uses latency signatures, stderr and HTTP evidence.
  - Owned runtimes are restarted and the request is retried, as long as no token has reached the client yet.
  - Runtimes hearth did not start are never killed.
- **Gateway.**
  - OpenAI, Anthropic Messages and Ollama APIs, translated as needed.
  - Cancellation is structural: when a client disconnects, the upstream is aborted and the slot freed.
  - One deadline covers queue time and every retry.
  - Errors are typed and carry a `remediation` field.
- **Scheduling.** Priority classes, fair queuing across apps, quotas and deadline-aware shedding.
- **For people and agents.**
  - `hearth doctor`: file descriptors, threads, swap, pressure.
  - `hearth incidents`: classifies panic and jetsam reports.
  - `hearth ports`: what is actually listening, and which model it serves.
  - `hearth fit`: does this model at this context fit right now?
  - All of the above is available as `--json` and as an MCP server.
- **Platforms.** macOS on Apple Silicon first. Linux with NVIDIA (NVML), AMD (sysfs) and CPU.

## License

Apache-2.0 OR MIT.
