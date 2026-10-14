# ADR-005: `hearth run` events in the journal, and notifications

- Status: **Accepted**
- Date: 2026-10-15

## Context

`hearth run` restarts servers that exit, wedge or poison themselves (ADR-004). It printed those events and kept nothing.

Two needs follow:
- **History.** Asked "did the vision server restart overnight, and why?", an agent had nothing to read.
- **Attention.** When hearth gives up on a crash loop, or a backend poisons itself, nobody sees it unless they are watching the terminal. The plan always called for a persisted event journal and for notifications. The user's notes show poisoned backends going unnoticed for hours.

## Decision

### Events go to the journal

- Every `hearth run` event is one snapshot of kind `run` in the journal from ADR-003. The body is the event, plus the address, the expected model, the command line and the time since the run started.
- `hearth log --kind run` (and the MCP `log` tool) lists them with a one-line summary: address, event, decision, command.
- `hearth diff --kind run` is refused with the reason. Events are not states, so there is nothing to compare.
- A journal that cannot be opened or written is reported on stderr; the server is still supervised.

### Notifications

There are two channels, both configured in `[notify]` and both off by default:
- **`desktop = true`.** A desktop notification: `osascript` on macOS, `notify-send` on Linux. The title and message are passed as arguments to a fixed AppleScript (`on run argv`), never spliced into it, so text from a server's log cannot be read as script.
- **`webhook_url`.** The event body is POSTed as JSON with `curl`, bounded by `notify.timeout_ms`. With `curl` doing the transfer, `https://` services such as ntfy or chat webhooks work, and hearth ships no TLS stack. The URL must be `http://` or `https://`; anything else is a configuration error.

Notifications fire on:
- a poisoned backend;
- a wedge;
- a lost process;
- any give-up decision, after exits, failed starts, a taken port, or never becoming ready.

Starts, readiness and noticed out-of-memory lines do not notify.

Each delivery runs on its own thread. A failure is reported on stderr; it never fails or delays supervision. Before `hearth run` exits, it waits for pending deliveries, because the give-up event is the last one and would otherwise be lost with the process.

## Consequences

- An agent answers "what happened to the server on 8081" with `log --kind run` instead of scrolling a terminal.
- Live on the development machine: a server that crash-looped against a local webhook receiver delivered the give-up event as JSON, and the journal held all six events in order.
- Notifications need `curl`, `osascript` or `notify-send`, which the target platforms ship. A missing tool is reported, not fatal.
- `doctor` does not notify yet: it runs when someone asks, so its reader is already there. The daemon will notify on pressure and descriptor trends, through the same module.
