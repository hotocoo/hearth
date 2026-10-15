# ADR-011: hearthd's dashboard, a web page on loopback

- Status: **Accepted**
- Date: 2026-10-15

## Context

ADR-009 chose a terminal app for `hearth top` and said "not a webpage". The user then asked for a premium web interface: one that works on desktop, tablet and phone, with keyboard control, motion that means something, and reduced-motion support. A terminal can do none of the layout or motion. This ADR reverses ADR-009 on that one point, at the user's request. `hearth top` stays and is polished alongside.

## Decision

- **Inside hearthd.** `[dashboard] listen` makes hearthd serve the page, beside `/metrics` and the gateway. It is the process already running at login, so there is nothing more to start. Off by default.
- **The same frame as `top`.** `GET /api/frame` returns what `hearth --json top` prints: the servers from hearthd's control socket, the machine from `doctor::measure` (not recorded), and the newest journal entries. Actions go through the control socket like `hearth restart`, so the same rules hold: only owned servers, and one order at a time.
- **No login, so four defenses.**
  1. `dashboard.listen` must be a loopback address; anything else is a configuration error.
  2. `Host` must name the listener (its address, or localhost, 127.0.0.1 or [::1] at its port). This stops DNS rebinding.
  3. An action (`POST`) must carry `X-Hearth: 1`. A cross-site page cannot set that header without a CORS preflight, and the dashboard never answers one. When the browser sends `Origin`, it must be the listener.
  4. The page, style and script are files built into the binary, under `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'`, with `nosniff` and `no-referrer`. Server output and journal notes are written by other programs, so they go into the page as text and never as markup; a unit test refuses `innerHTML` in the script.
- **No new dependencies, no external assets.** Three static files, std-only HTTP sharing the accept loop with `/metrics` and the gateway (`listen.rs`). The fonts are system ones on purpose: a local tool must work offline and paint at once. Numbers are set in the monospace face so they do not jitter as they change.
- **Design.** Neutral graphite and paper, one emerald accent, and colour otherwise only for state (ready, starting, stopped, gave up) and severity. Memory leads at display size; limits follow smaller. Servers are rows, not cards, with their actions inline. Findings sit beside the journal on a wide screen and stack on narrow ones. Light and dark follow the system.
- **Motion with a reason.**
  - Sections enter once, staggered, when first painted or scrolled to.
  - A new server row or journal entry arrives with a short slide.
  - Meters move with an ease-out on `transform`.
  - The output drawer slides in from the right (a bottom sheet on phones) with a spring curve, and the page behind it is made `inert`.
  - A starting server's dot breathes; nothing else loops.
  - Refreshes patch the page in place, so nothing re-animates every two seconds.
  - `prefers-reduced-motion` turns all of this off.
  - There is no parallax: on an instrument panel it would move data the reader is trying to read.
- **Contrast.** Text and badge colours meet WCAG AA (4.5:1) in both themes; the ratios were computed, not eyeballed.
- **Keyboard.** The keys of `top` (arrows or j/k, r, s, S, l), Esc, and `?` for a shortcuts dialog. Stop asks once more ("Confirm stop") before it frees a model's memory. Buttons are real buttons with visible focus rings, and there is a skip link.
- **Refresh.** Every `dashboard.refresh_ms`, five times slower while the tab is hidden, and three times slower after a failure. The top bar says whether hearthd is answering.

## Consequences

- `open http://127.0.0.1:9471/` is a live view of the box that anyone at the machine can act from, including on a phone through a tunnel they set up themselves; hearth opens nothing beyond loopback.
- The end-to-end test fetches the page and checks its CSP, reads the frame, is refused for a foreign `Host`, a missing `X-Hearth` and a foreign `Origin`, and restarts the server from the API.
- The findings show one sample. Growth projections need two, so they appear in hearthd's recorded watch rounds (the journal) but not in the findings list. Showing the latest recorded round's findings there is the next step.
- Checked by eye in headless Chrome at 1440, 820, 390 and 375 px, light and dark, with the drawer open. That is a look, not a test: no browser automation runs in CI.
