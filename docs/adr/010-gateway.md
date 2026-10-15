# ADR-010: hearthd's gateway, one address in front of its servers

- Status: **Accepted** (first slice)
- Date: 2026-10-15

## Context

Each server hearthd keeps listens on its own port. A client pointed at one of them sees a refused connection while it restarts, a hang while it is wedged, and a server that keeps generating after the client gave up. The README's planned gateway covers all of this plus translation, retry, admission and scheduling; this ADR lands the part that needs none of those.

## Decision

- **Routing by name.** With `gateway.listen` set, hearthd forwards a request for `/s/NAME/REST` to server NAME as `/REST`. A client's base URL is `http://LISTEN/s/NAME/v1`; request and answer pass through unchanged, streaming included.
- **Typed refusals.** Anything hearth can answer itself comes back as `{"error": {"type", "message", "remediation"}}`: `not_found` (404) for a path outside `/s/` or a name hearthd does not keep, listing the names it does; `not_ready` (503) for a server that is stopped (`hearth start NAME`), given up on, unsupervised or still starting; `unreachable` (502) when its port refuses; `timeout` (504) when it sends nothing for `gateway.idle_timeout_ms`; `busy` (503) past `gateway.max_clients`.
- **Structural cancellation.** One thread copies the request up, one the answer down. When the client closes, the server's connection is closed; when the server finishes or goes silent, the client's is. llama.cpp stops a generation when its connection goes.
- **std only.** A thread per request, like `/metrics`. No async runtime enters the static builds.
- **One request per connection.** `Connection: close` is forced on the forwarded head and hop-by-hop headers dropped. A shortcut, marked in code, until keep-alive matters.

Not in this slice: API translation, retry before the first token, admission from measurement (blocked on ADR-000's gate), the request-level failure classifier, priority and fair scheduling.

## Consequences

- Clients stop knowing ports: a server can move or restart behind the same URL, and a stopped one says how to start it.
- The end-to-end test forwards to the fake server, gets 404 and 503 with their remediations, and drops a streaming client mid-answer: the server sees its connection close.
- Live, through a hearthd owning a real llama-server (llama.cpp b11429, Qwen2.5 0.5B Q4_K_M): an OpenAI chat completion and a streamed one came back, and so did an Anthropic `/v1/messages` call. A client that dropped a 1500-token stream mid-answer left llama.cpp's slot idle 0.07 s later. After `hearth stop`, the same URL answered 503 with `hearth start tiny`.
