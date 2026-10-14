# hearth

**A control plane for local inference.** One daemon in front of every model runtime on a machine (llama.cpp, MLX, Ollama, LM Studio, ComfyUI, anything OpenAI-compatible), so that many apps can share one box without it OOMing, wedging or panicking.

> Status: **P2 in progress.** Shipped: `hearth-probe` (measurement instrument), the `hearth` CLI (inspection, history, supervision, workflows and an MCP server; `hearth --help` lists every command) and `hearthd`, which keeps configured model servers alive, checks they fit before starting them, and watches the machine. hearthd does not proxy inference yet; nothing below the "Planned" line has shipped. Each phase lands implemented, tested on real runtimes, and documented ([docs/adr](docs/adr)), or it does not land.

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

Release archives (macOS arm64, Linux x86_64 and aarch64, static) are built by `.github/workflows/release.yml` and attached to each `v*` GitHub release. Until the first release, build from source:

```sh
cargo build --release -p hearth
./target/release/hearth doctor              # memory, open files, threads vs kernel limits; who holds them
./target/release/hearth doctor --watch 30   # project time to each limit from measured growth
./target/release/hearth doctor --explain    # where every number comes from: counter, sysctl, formula
./target/release/hearth incidents           # macOS: panics (watchdog / GPU driver), jetsam, stalls; Linux: OOM kills, hung tasks
./target/release/hearth fit model.gguf --ctx 32768 --cache-type q8_0   # fits the device? fits now? max context?
./target/release/hearth fit qwen3:8b --ctx 32768 --cache-type q8_0     # an Ollama model, read from its store
./target/release/hearth fit org/repo-GGUF:Q6_K --ctx 32768 --cache-type q8_0   # from the Hugging Face cache or LM Studio
./target/release/hearth fit --port 8080 --ctx 131072 --cache-type q8_0 # the model a running server serves: could it take this context?
./target/release/hearth fit --port 8080 --cache-type q8_0            # ...or does its current context fit at all?
./target/release/hearth ports --runtimes    # which model runtime answers on which port, with memory
./target/release/hearth log                 # snapshots ports recorded, newest first (--kind doctor)
./target/release/hearth diff                # what changed since the run before: restarts, wedges, models, memory
./target/release/hearth diff --kind doctor --from ~5   # per-process file, thread, CLOSE_WAIT movement
./target/release/hearth follow ollama       # one process/port through every snapshot: appeared, changed, restarted, gone
./target/release/hearth follow llama --kind doctor     # its open files, threads, CLOSE_WAIT over time: a leak at a glance
./target/release/hearth show --kind doctor ~3          # one snapshot in full
./target/release/hearth tag healthy         # name this moment; kept past retention
./target/release/hearth diff --from healthy # everything that differs from when it was fine
./target/release/hearth diff --kind doctor --from @2h  # against how the machine was two hours ago
./target/release/hearth log --since 30m     # only what was recorded in the last 30 minutes
./target/release/hearth log --stats         # how much history each kind keeps, how far back it reaches
./target/release/hearth run --port 8081 --expect qwen3 --model qwen3.gguf --ctx 32768 --cache-type q8_0 \
    -- llama-server -m qwen3.gguf -c 32768 -ctk q8_0 -ctv q8_0 --port 8081   # --model/--ctx/--cache-type: refuse what cannot fit
#   keeps the server alive: restarts on exit, wedge, or a poisoned backend in its log (the latched
#   Metal "recreate the backend" state that still answers /health), SIGTERM then SIGKILL, gives up on a crash loop,
#   refuses a port someone else serves, and the server dies with hearth even on SIGKILL
./target/release/hearth log --kind run      # every start, exit, wedge, poisoning and give-up hearth run saw
./target/release/hearth watch               # doctor + ports on intervals, all recorded; reports pressure, limits
#                                             reached within the horizon, runtimes that vanish or go silent
./target/release/hearth config supervise    # every setting in effect, its source and meaning; no number lives in code
./target/release/hearth bundle > box.json   # everything above in one document, secrets redacted (process names stay)
./target/release/hearth workflows           # recurring questions as tool-by-tool recipes; add your own in workflows.json
./target/release/hearth completions zsh > ~/.zfunc/_hearth   # tab completion (bash, fish, zsh, elvish, powershell)
./target/release/hearthd                    # keeps every [servers.NAME] alive like hearth run, and watches like hearth watch
./target/release/hearth status              # what hearthd keeps: state, pid, restarts, last event
./target/release/hearth restart vision      # restart one now (owned servers only)
./target/release/hearth stop vision         # free its memory; it stays down until `hearth start vision`
./target/release/hearth logs vision         # its last lines of output, across restarts
./target/release/hearth reload              # apply [servers] edits: new start, removed stop, the rest keep running
./target/release/hearth service install     # run hearthd at login: launchd on macOS, systemd user unit on Linux
#   [metrics] listen = "127.0.0.1:9469" in your config: hearthd serves Prometheus metrics at /metrics
```

Every command takes `--json`. Every finding carries a `remediation`. Every setting, with its default and meaning, is in [docs/configuration.md](docs/configuration.md).

Set `[notify] desktop = true` or `webhook_url` in your config to be told when `hearth run` finds a poisoned backend or a wedge, or gives up on a crash loop, and when `hearth watch` finds a problem.

Full runs of `ports` and `doctor` are recorded in hearth's journal (`$HEARTH_STATE`, else `$XDG_STATE_HOME/hearth`, else `~/.local/state/hearth`). `log` and `diff` read it back, so "what changed on this box since I last looked" is one command. `ports` also learns each runtime's probe deadline from its recorded answers, and flags a runtime that has become much slower than its own history.

For AI agents, `hearth mcp` serves the same commands as MCP tools over stdio: `doctor`, `incidents`, `fit`, `ports`, `log`, `diff`, `show`, `follow`, `tag`, `status`, `restart`, `stop`, `start`, `logs`, `reload`, `config`, `bundle` and `process_memory`. `restart`, `stop` and `start` reach only servers hearthd started. It also serves five workflows as MCP prompts: `diagnose-slow-machine`, `server-died`, `can-i-load`, `what-changed` and `find-a-leak`. Each says which tools to call, in which order, and how to answer. [docs/agents.md](docs/agents.md) maps questions to tools. For Claude Code:

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
INTERVAL_MS=100 CTX=8192 BASELINE_S=60 SETTLE_S=20 RECLAIM_S=30 FILL_TOKENS=4096 FILL_TIMEOUT_S=600 \
  scripts/p0-gate.sh runs/$(date +%s) small.gguf larger.gguf
./target/release/hearth-probe report runs/<id>/samples.jsonl --criteria docs/adr/000-criteria.json
```

## Planned

All of this is design, not shipped code. The design is in the ADRs as each phase lands.

- **Admission from measurement.** Blocked on the P0 gate (ADR-000), which has not run yet; until it does, `hearth fit`'s "fits right now" is marked provisional.
  - Each memory pool gets a budget from the device ceiling, the measured resident footprints and a measured reserve.
  - Load cost comes from GGUF geometry and is corrected by the footprint learned per model.
  - If measurement fails, hearth serializes. It never fails open.
- **Recovery.**
  - A classifier sorts failures as transient, capacity, poisoned, wedged or model-missing. It uses latency signatures, stderr and HTTP evidence.
  - Owned runtimes are restarted and the request is retried, as long as no token has reached the client yet. Restart on exit, wedge or a poisoned backend, with the SIGKILL escalation and crash-loop limit, already ships in `hearth run` and `hearthd` (ADR-004, ADR-007). The request-level classifier and the retry come with the gateway.
  - Runtimes hearth did not start are never killed.
- **Gateway.**
  - OpenAI, Anthropic Messages and Ollama APIs, translated as needed.
  - Cancellation is structural: when a client disconnects, the upstream is aborted and the slot freed.
  - One deadline covers queue time and every retry.
  - Errors are typed and carry a `remediation` field.
- **Scheduling.** Priority classes, fair queuing across apps, quotas and deadline-aware shedding.
- **For people and agents.**
  - MCP `load`, `unload` and `drain` on runtimes hearth owns, next to the `restart` that ships. Runtimes hearth did not start are refused, with the reason.
- **Platforms.** macOS on Apple Silicon first. Linux with NVIDIA (NVML), AMD (sysfs) and CPU.

## License

Apache-2.0 OR MIT.
