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

## Addendum (2026-10-15): held while starting

- A request for a server that is `starting`, or `waiting` out a restart hearthd decided, is held for up to `gateway.ready_wait_ms` (polling every `gateway.ready_poll_ms`) and forwarded once the server is ready. A client that arrives mid-restart, including one after a poisoned backend or a wedge, is then served late rather than refused.
- A server that is `stopped`, `gave_up` or `unsupervised` does not come back by itself, so its request is still refused at once with what to do.
- The request head is already read when it is held, and the body has not been read yet: it streams to the server once forwarding starts, so holding costs no buffering.
- The end-to-end test restarts the server and sends a request through the gateway straight away; it is answered 200.

## Addendum (2026-10-15): retry before the first byte

- **What is retried.** A request whose body has a `Content-Length` within `gateway.retry_body_bytes` is read whole before it is sent, so it can be sent again. Chunked and larger bodies stream through once, as before.
- **When.** Only while nothing has reached the client:
  - the connection is refused, reset or closed before an answer;
  - or the answer is a 5xx and hearthd then restarts the server, which is how a poisoned Metal backend shows.
- **Only when hearthd notices.** The gateway retries only when hearthd, within `gateway.retry_grace_ms`, reports the server out of `ready` or back with a new pid. Then it waits for the server to be ready and sends the request again, at most `gateway.retries` times.
  - A failure hearthd never notices is not one a retry can fix, so it is reported.
  - A 5xx from a server that stays as it was is the application's answer, and is passed on.
- **Silence is not retried.** A server silent past `idle_timeout_ms` gets the 504. It is wedged, and hearthd's restart of it should not double the client's wait.
- **One deadline.** `gateway.ready_wait_ms` covers waiting for the server and every retry.
- **Test.** In the end-to-end test the fake server exits on its first `POST /flaky`. hearthd restarts it, and the client receives 200 with its own body echoed back, from the second attempt.

## Addendum (2026-10-15): fair queuing

- **A limit per server.** `gateway.per_server_concurrency` caps how many requests one server runs at once through the gateway. Set it to the server's slots (`-np`). 0, the default, leaves queuing off.
- **Round robin across apps.** When a server is full, waiting requests are let in round robin across apps, named by the `X-Hearth-App` header, and in arrival order within one app. A batch of a hundred requests from one app holds up another app's request by one turn, not by the whole batch. Requests without the header share one app.
- **One deadline.** Time in the queue counts toward `gateway.ready_wait_ms`, like waiting for a restart and retrying. A request still waiting at the deadline gets a 503 `queue_full`, saying how many were running or ahead.
- **Priorities, quotas and shedding by deadline** are not built. They need real traffic from several apps to set sensibly.
- **Tests.** Unit tests check the order of turns across and within apps, a request that gives up leaving the line, and a full server letting the next request in when one finishes.

## Addendum (2026-10-15): Ollama's API translated

- **Why.** llama.cpp answers OpenAI's and Anthropic's APIs but not Ollama's: `/api/chat`, `/api/generate`, `/api/tags` and `/api/version` all return 404 from llama-server (checked on b11429). Many tools only speak Ollama.
- **What.** For those four endpoints the gateway first asks the server's `/api/version`. A server that answers it speaks Ollama, and gets the request as is. Otherwise:
  - `/api/chat` and `/api/generate` become `/v1/chat/completions`; `generate`'s `system` and `prompt` become messages.
  - Options map to their OpenAI names (`num_predict` to `max_tokens`, `temperature`, `top_p`, `top_k`, `min_p`, `seed`, `stop` and the penalties); others are dropped. `format` becomes `response_format`.
  - Images become data-URL content parts. Tool definitions pass through, and tool calls come back with their arguments as objects. Streamed tool-call fragments are gathered and sent whole on the last line.
  - Streamed answers come back as NDJSON: one line per piece, then a `done` line with `done_reason`, `prompt_eval_count`, `eval_count` and `total_duration`.
  - `/api/tags` lists `/v1/models`, and `/api/version` answers hearth's own version.
- **Bounds.** A translated request's body is read whole, up to `gateway.translate_body_bytes`. A translated request is not retried; if the server turns out to speak Ollama, the body goes through the normal retrying path.
- **Checked.**
  - Against a real llama-server (Qwen2.5 0.5B, `--jinja`): chat whole and streamed, generate, tags, and a tool call, both whole and streamed.
  - With the official `ollama` Python client: `list`, `chat`, streamed `chat` and `generate`.
  - The end-to-end test talks to a fake OpenAI-only server, both whole and streamed.
