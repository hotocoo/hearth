# hearth for AI agents

hearth gives an agent the facts about a machine that runs local models: what is running, what it uses, what happened and what changed. With these facts it does not have to grep logs and guess which pid owns which port. Everything works through MCP, and every MCP tool is also a CLI command with `--json`.

## Register

```sh
cargo build --release -p hearth
claude mcp add hearth -- "$(pwd)/target/release/hearth" mcp
```

Any MCP client works: `hearth mcp` speaks JSON-RPC over stdio, protocol revisions 2024-11-05 to 2025-11-25.

## Which tool for which question

| Question | Tool |
|---|---|
| Why is the machine slow or unstable? | `doctor` (with `watch_seconds` for growth), then `diff` with kind `doctor` |
| What runs where, and is it alive? | `ports` (with `runtimes_only` for model servers only) |
| Did the machine crash, or kill something? | `incidents` |
| Which model on disk should I run for a 32k-token task? | the `what-can-i-run` prompt, or `models` with `ctx` and `serving` |
| What models are on this machine, and what context does each fit at? | `models` (with `cache_type` to size GGUF contexts) |
| Will this model fit? | `fit`: a `.gguf` path, an MLX or safetensors directory, `org/repo:QUANT` (Hugging Face cache, LM Studio), an Ollama name (`qwen3:8b`), or the `port` of a running server (without `ctx`, its own context) |
| What changed since earlier? | `diff` (kind `ports` or `doctor`, `from` set to `~N`, `@2h` or a tag) |
| How much history is there? | `log` with `stats` |
| When did this error, model or pid first or last show up? | `log` with `grep` (each match keeps its `~N` for `show` and `diff`) |
| When did this process start leaking or restarting? | `follow` with a name, kind `doctor`, `ports` or `run` |
| Everything recorded at one moment | `show`, with `pointer` (`/body/health/memory`) for only the part you need |
| Mark a known-good state to compare with later | `tag` |
| Record what you changed or advised, and why | `note` with `by`; `log` with kind `note` reads them back |
| Servers hearth keeps alive | `status`, `logs`, and `restart`, `stop`, `start` (owned servers only) |
| Why hearth waited, retried or reported something | `config`; `doctor` with `explain` for where each number comes from |
| The whole picture in one call | `bundle` |

## Resources

Recorded snapshots are MCP resources, read with `resources/read`:
- `hearth://ports/~0`: the latest ports snapshot;
- `hearth://doctor/@1h`: the doctor snapshot from an hour ago;
- `hearth://run/healthy`: the run snapshot at the tag `healthy`.

`resources/list` offers the latest snapshot of each kind and every tag. The template `hearth://{kind}/{reference}` takes any reference that `diff` takes.

## Workflows

`hearth mcp` also serves MCP prompts. Each is a workflow: which tools to call, in which order, and how to answer.

- `diagnose-slow-machine`: the resource running out, who holds it, what to do.
- `server-died`: exit, wedge, poisoned backend, memory kill or crash loop, and the fix.
- `can-i-load`: fits the device, fits now, what would have to be unloaded.
- `what-changed`: since N snapshots ago or since a tag.
- `find-a-leak`: which process, which resource, how fast, how long until the limit.

### Your own workflows

Put a JSON array in `workflows.json` beside your hearth config (`~/.config/hearth/workflows.json` by default). Each entry has a `name`, a `description`, a `template` and optional `arguments` with `name`, `description`, `required` and `default`. The template refers to tools in backticks and to arguments as `{name}`. A workflow with the name of a built-in one replaces it.

A workflow that names a tool hearth does not have is skipped, and `hearth workflows` says why. `hearth workflows NAME --arg k=v` prints one, filled in, for use outside MCP.

```json
[{ "name": "morning-check", "description": "Daily look at the box.",
   "template": "Call `doctor`, then `diff` with kind \"doctor\" and from \"{since}\", then `status`.",
   "arguments": [{ "name": "since", "description": "How far back.", "default": "@1d" }] }]
```

## Rules hearth keeps, and agents should too

- `ports` marks each listener with `owned_by`: the hearthd server name, or null. Act only on owned ones.
- hearth never touches a process it did not start. `restart`, `stop` and `start` reach only servers configured under `[servers]` in hearthd. A server someone else runs is reported, never signalled.
- Nothing loads a model. `fit` reads file headers; `ports` sends read-only status GETs.
- Every number hearth uses is in its configuration, with its meaning (`config`). A finding states the measurement it came from.
- History exists only for what was recorded. `ports`, `doctor`, `hearth watch` and `hearthd` record as they go. If `diff` finds too little, record now and say that history starts here.
