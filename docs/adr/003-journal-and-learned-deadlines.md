# ADR-003: Journal, history and learned probe deadlines

- Status: **Accepted**
- Date: 2026-10-14

## Context

Every `hearth` command answered "what is true now". The questions agents and people ask after an incident are about change:
- what restarted since this morning;
- which runtime stopped answering;
- whose open files keep growing.

Before this, answering any of them meant running a command twice and comparing the outputs by eye.

ADR-002 also promised that a runtime's measured answer time would replace `first_contact_timeout_ms`. That needs somewhere to keep measurements between runs.

## Decision

### Journal

`hearth-core::state` keeps one JSON document per observation in the state directory:
1. `$HEARTH_STATE`;
2. else `$XDG_STATE_HOME/hearth`;
3. else `~/.local/state/hearth`.

Empty and relative `XDG_STATE_HOME` and `HOME` values are ignored, as the XDG specification says. A relative `HEARTH_STATE` is an error, because it was asked for explicitly. The directory is created with mode 0700, because snapshots hold process lists and executable and model paths.

- **File names.** A file is named `<ns since epoch>-<pid>-<kind>.json`. Names sort by time. Ids are strictly increasing within a process, because macOS clocks tick in microseconds.
- **Writes.** Each document goes to a hidden temporary file, is fsynced, then renamed into place. Concurrent `hearth` processes never interleave, and readers never see a partial document. No lock is needed, and the MSRV has no `File::lock`. A failed write removes its temporary file.
- **Retention.** `state.max_snapshots_per_kind` snapshots are kept per kind, so doctor runs never push port history out. The oldest go first.
  - A concurrent prune that loses the race is ignored.
  - A file pruned between listing and reading is skipped.
  - A prune that fails is a warning on a snapshot already written, not a failed write.
- **References.** A snapshot is named by its full id or by `~N`, meaning N before the latest. MCP clients supply these, so:
  - ids are validated (digits, digits, `[a-z_]` kind) before they touch the file system;
  - an id of another kind is rejected;
  - an `N` too large to count back is "not found", not an overflow.

The daemon will move this to an embedded database. The one-file-per-snapshot layout was chosen as the simplest one that is safe with concurrent writers and needs no new dependency.

### Recording policy

Full runs of `ports` and `doctor` always record. This is a read command with a side effect, accepted because:
- nobody has to remember to snapshot before an incident;
- learning needs every answer.

`ports --runtimes` is not recorded: it is a partial picture, and a later diff would report every other port as vanished. When the journal cannot be read or written, the report is still complete and lists the reason in `state_errors`.

### History commands

- `hearth log [--kind ports|doctor] [--limit N]` lists snapshots newest first, each with a one-line summary.
- `hearth diff [--kind] [--from REF] [--to REF] [--limit N]` defaults to `~1` → `~0`.
- Both commands are also MCP tools.

**Ports diff.** Ports are keyed by program (executable path, else process name) and port, not by pid. Each key holds the set of pids listening there, because prefork servers and `SO_REUSEPORT` workers share one socket. It reports:
- `appeared` and `vanished`;
- `restarted` (same program and port, a different pid set);
- `changed`: a different identity description, which covers health, loaded models, n_ctx and "stopped answering".

How long a silent port was waited for is not a change, and probe errors are described without timings for the same reason. Footprint deltas are given for runtimes whose pid survived.

**Doctor diff.** It reports pressure-level changes, plus deltas for:
- available memory, swap used, kernel-wide open files and threads;
- per process, matched by pid and name: open files, threads and `CLOSE_WAIT`.

A leak shows as steady growth across `diff --from ~N` windows. `--limit N` keeps the N largest movements of each unit, so byte deltas, whose magnitudes are always larger, cannot crowd out a growing count.

Snapshots are read field by field, not as typed structs, so snapshots written by other hearth versions still compare.

### Learned probe deadlines

For a `(program, port)` with at least `discovery.learned_min_samples` recorded answers:

```
deadline = min(first_contact_timeout_ms,
               max(learned_deadline_floor_ms, slowest recorded answer × learned_deadline_multiplier))
```

- **Floor.** Loopback runtimes often answer in well under a millisecond. Ten times that is less than the scheduling jitter of a machine probing every port at once, so `learned_deadline_floor_ms` bounds it from below. The configuration rejects a floor above the first-contact timeout.
- **First request only.** Answer times are measured on the first request (`/api/version`), so the learned deadline bounds that request alone. The requests after it ask for model lists and slot state, which take longer, and keep the first-contact deadline.

- **Why the slowest answer, not a quantile.** Spikes this program has already shown once do not count against it again, and it takes one knob fewer.
- **Units.** Answer times are recorded in microseconds (`latency_us`), because loopback answers often take less than a millisecond.
- **Second chance.** A runtime that misses its learned deadline is probed again with the first-contact deadline before it is called unresponsive. A learned deadline therefore never creates a false "wedged". If it answers the second time, it is reported as `slow` with a warn finding, because a server slowing far past its own history is how a wedge often starts.
- **Visibility.** Every port reports its `deadline` with `source` (`first_contact` or `learned`), the sample count, and the slowest answer.
- **Validation.** The configuration rejects a multiplier below 1 or not finite, zero samples, a zero timeout, a floor above the timeout, and zero retention.

## Consequences

- An agent can answer "what changed on this machine" with `diff` instead of comparing two outputs itself.
- On the development machine, after nine runs, 15 ports had learned deadlines. Examples:
  - ControlCenter's AirPlay port: 180 ms, from a slowest answer of 18 ms.
  - The user's llama-server: the 50 ms floor, from a slowest answer of 2.6 ms.

  The first-contact deadline is 2.5 s.
- A port that is silent from the start still waits the full first-contact deadline. Learning speeds up classification only for programs that have answered before.
- The journal grows to at most `max_snapshots_per_kind` files per kind. A doctor snapshot holds every process's counts, about the size of `hearth doctor --json`.
