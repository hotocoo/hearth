# ADR-009: `hearth top`, one screen to watch and steer hearthd

- Status: **Accepted**
- Date: 2026-10-15

## Context

hearthd (ADR-007, ADR-008) keeps servers alive and watches the machine, but seeing it meant running `hearth status`, `hearth doctor` and `hearth log` over and over, and `/metrics` needs a Prometheus to read it. People wanted one app to keep open while they work, that agents can drive too, not a webpage.

## Decision

- **A terminal app.** `hearth top` redraws one screen: servers (as `hearth status` prints them), the machine (doctor's summary line and findings), and the newest journal entries. Keys act on the selected server through the same control socket the CLI uses: `r` restart, `s` stop, `S` start, `l` its output. `q` or Ctrl-C quits.
- **No new dependencies.** The screen is driven with escape sequences (alternate screen, clear, cursor) and `stty` for raw mode and size. A GUI toolkit would add hundreds of crates to the static release builds, and a webview is a webpage. The workspace forbids `unsafe`, so termios is reached through `stty`. The terminal is put back on drop, so an error or panic leaves it usable, and Ctrl-C arrives as a key rather than a signal.
- **Nothing measured twice.** Each refresh reuses `control::call`, `doctor::measure` and `hearth log --all`. `doctor::measure` builds the doctor report without recording it, so an open `top` does not write a snapshot every two seconds; hearthd's watch records rounds already.
- **Agents use the same frame.** `hearth --json top` prints the frame as one document, and `--once` as text. There is no new MCP tool: agents act through the existing commands, and their restarts and notes appear in the journal section on the next refresh.

## Consequences

- One `hearth top` beside hearthd shows what the daemon and any agent are doing, and is enough to restart or stop a server by hand.
- The end-to-end test drives `top` on a pseudo-terminal from `script(1)` (BSD and util-linux syntax differ), presses `r`, waits for the server's new pid, and checks the alternate screen is left on `q`.
- Each refresh spawns `stty size`, so a resized terminal is drawn to its new size on the next refresh, not at once.

## Addendum (2026-10-15): colour, and stop asks twice

- **Colour with a meaning.** On a terminal, each server gets a dot in its state's colour (green ready, yellow starting, red gave up, dim stopped), findings their severity's, the selected row is bold and section titles stand out. `NO_COLOR` turns it off, and `--once` and `--json` never colour. Lines are cut by what shows, not by the bytes of their escape codes, and a cut line resets its style.
- **Stop asks twice.** A first `s` says what stopping does; a second `s` on the same server stops it. Any other key disarms it. The dashboard (ADR-011) asks the same way.
