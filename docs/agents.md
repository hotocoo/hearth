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
| Will this model fit? | `fit`: a `.gguf` path, an Ollama name (`qwen3:8b`), or the `port` of a running server |
| What changed since earlier? | `diff` (kind `ports` or `doctor`, `from` set to `~N` or a tag) |
| When did this process start leaking or restarting? | `follow` with a name, kind `doctor`, `ports` or `run` |
| Everything recorded at one moment | `show` |
| Mark a known-good state to compare with later | `tag` |
| Servers hearth keeps alive | `status`, `logs`, and `restart`, `stop`, `start` (owned servers only) |
| Why hearth waited, retried or reported something | `config` |
| The whole picture in one call | `bundle` |

## Workflows

`hearth mcp` also serves MCP prompts. Each is a workflow: which tools to call, in which order, and how to answer.

- `diagnose-slow-machine`: the resource running out, who holds it, what to do.
- `server-died`: exit, wedge, poisoned backend, memory kill or crash loop, and the fix.
- `can-i-load`: fits the device, fits now, what would have to be unloaded.
- `what-changed`: since N snapshots ago or since a tag.
- `find-a-leak`: which process, which resource, how fast, how long until the limit.

## Rules hearth keeps, and agents should too

- hearth never touches a process it did not start. `restart`, `stop` and `start` reach only servers configured under `[servers]` in hearthd. A server someone else runs is reported, never signalled.
- Nothing loads a model. `fit` reads file headers; `ports` sends read-only status GETs.
- Every number hearth uses is in its configuration, with its meaning (`config`). A finding states the measurement it came from.
- History exists only for what was recorded. `ports`, `doctor`, `hearth watch` and `hearthd` record as they go. If `diff` finds too little, record now and say that history starts here.
