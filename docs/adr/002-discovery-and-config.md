# ADR-002: Runtime discovery and layered configuration

- Status: **Accepted**
- Date: 2026-10-14

## Context

Every app on the development machine had its own idea of which port held which model:
- the ITSM JVM and a llama-server both wanted 8080;
- a vision shim on 8766, embeddings on 8768, rerank on 8769;
- Ollama on 11434, until it served only one model.

Agents diagnosing these spent turns guessing pid ownership. Hardcoded port lists break the moment anything moves, and the project rules forbid them anyway.

The same rules forbid numbers in code. Probing a port still needs a deadline, because a wedged server accepts the connection and never answers.

## Decision

### Discovery is by observation, not by list

1. Enumerate listening TCP sockets per process:
   - macOS: `proc_pidfdinfo` socket info through the `libproc` crate's structs;
   - Linux: `/proc/<pid>/net/tcp{,6}` joined with the process's socket inodes.

   No root is needed. Other users' processes are counted as uninspectable.
2. Probe each port once, preferring IPv4. Wildcard binds are probed via loopback. Probes run in parallel, one worker per available core.
3. Identify by what answers, in this order (ported from ah's discovery):
   - Ollama: `/api/version` returns a version string, then `/api/tags` and `/api/ps` for models and resident sizes.
   - llama.cpp: `/props` has `default_generation_settings`, then `/health` for readiness and `/v1/models` for router mode. `/v1/models` answers 200 while a model is still loading, so it is never used for readiness.
   - ComfyUI: `/system_stats`.
   - SD WebUI API: `/sdapi/v1/sd-models`.
   - LM Studio: `/api/v0/models` with per-model `state`.
   - OpenAI-compatible: `/v1/models`.
4. Nothing more is sent to a port that answered non-HTTP or did not answer. The probe client is a bounded HTTP/1.1 GET (Content-Length and chunked), limited by a deadline and a byte cap.

Ports that accept connections and never answer are reported:
- as **warn** when the same process serves a model on another port (a likely wedge);
- as **info** otherwise (perhaps a non-HTTP protocol).

### Configuration

The repository ships `crates/hearth-core/config/hearth.toml`. The binaries embed it as the base layer. A user file (`$HEARTH_CONFIG`, or `$XDG_CONFIG_HOME/hearth/hearth.toml`) is merged over it key by key. Unknown keys are errors, so a typo cannot silently fall back. No value lives in Rust code.

Values in the file are first-contact values only. The daemon will replace them with learned latency once a runtime has answered; that store arrives with the daemon.

## Consequences

- `hearth ports` (and the MCP tool `ports`) answers "what runs where, which model, how much memory, is it alive" in one call.
- On the development machine it found Docker Desktop's model runner speaking the Ollama API on port 12434, which nobody had listed.
- The same probe code will feed the daemon's supervisor, so CLI and daemon cannot disagree about identity.
