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

## Addendum (2026-10-15): `show` and `follow`

The journal reads like a version history, so two more commands follow git:

- **`hearth show [--kind K] [REF]`** prints one snapshot in full, as it was written, like `git show`.
- **`hearth follow PATTERN [--kind K] [--all]`** follows everything whose name, pid, address, path or server name contains the pattern through every snapshot, oldest first, like `git log --follow`. It lists:
  - when the match appeared;
  - each change in its state (ports: state and footprint; doctor: open files, threads and `CLOSE_WAIT`; run: events);
  - when it went away.

  `--all` lists every snapshot, not only changes.

Both are MCP tools. MCP tool calls are now translated to the CLI from each tool's own definition in `mcp-tools.json` (its `cli` entry names positional arguments and renamed flags). Adding a tool needs no Rust beyond the command itself, and unknown arguments are refused rather than passed through.

On the development machine, `follow llama-server` showed the user's server footprint going from 34.5 GiB to 35.0 GiB and then to 24.2 GiB. `follow remoting --kind doctor` showed a remote-desktop host's open files climbing from 41 to 54.

## Addendum (2026-10-15): tags

`hearth tag NAME` names the latest snapshot of every kind (ports, doctor, run), like `git tag`.
- **Use.** The name works wherever a snapshot is asked for, for example `diff --from healthy` or `show healthy`.
- **Retention.** Tagged snapshots are never pruned and do not count against `state.max_snapshots_per_kind`.
- **Move and delete.** Tagging again moves the name. `--delete` releases it.
- **Names.** Names start with a lowercase letter, then letters, digits, `-`, `_` or `.`, without `..`. So a tag can never be mistaken for an id or a `~N` reference, and is a safe file name. Tags live in `tags/NAME.json` in the state directory.

The `what-changed` workflow accepts a tag.

## Addendum (2026-10-15): time references

A snapshot can also be named `@AGE`, as in `@90s`, `@30m`, `@2h` or `@1d`: the latest snapshot taken at least that long ago, which is how things were then. This is git's `HEAD@{2.hours.ago}`. `hearth log --since AGE` lists only what was recorded within that time. The time comes from snapshot ids, which begin with their nanosecond timestamp, so no file is read to find it.

## Addendum (2026-10-15): retention by age

Retention by count alone kept 200 snapshots. At hearthd's 10-second doctor interval that is about 33 minutes, so `@1d` had nothing to point at. Snapshots of kinds listed in `state.thinned_kinds` (ports, doctor) are now thinned by `state.tiers`. Each tier is a window back from now in which one snapshot is kept per `every_ms` (all of them when 0): the newest snapshot of each interval. Past the last window, snapshots are deleted.

The defaults keep:
- everything for an hour;
- one per 10 minutes for a day;
- one per hour for a week.

That is about 700 doctor snapshots, some 45 MB at tens of KB each. `max_snapshots_per_kind` (now 1000) stays a hard cap over everything.

Kinds not listed, such as `run` events, are only capped, because every event matters. Tagged snapshots are exempt as before. Configuration rejects tiers whose windows do not grow.

## Addendum (2026-10-15): snapshots as MCP resources

`hearth mcp` declares the `resources` capability. Each snapshot is `hearth://KIND/REFERENCE`, where KIND is ports, doctor or run and REFERENCE is an id, `~N`, `@AGE` or a tag. The reference goes through the journal's own resolution, so it is validated before it touches the file system. `resources/list` offers the latest snapshot of each kind and each tag.

## Addendum (2026-10-15): grep, tags on one snapshot, notes

`hearth log --grep TEXT` lists the snapshots that mention TEXT, newest first, read lazily until `--limit` matches. Matching follows the recorded values, not their escaped JSON text: keys and strings match as substrings with case ignored, and numbers match only when equal, so pid 123 does not match 1234. Each match keeps its `~N` within its kind. A workflow that keeps working while snapshots are recorded refers to them by id, because a new recording shifts every `~N`.

`hearth tag NAME --at REF` names one snapshot, like `git tag NAME COMMIT`. It is how a moment found by grep is kept past retention. A snapshot pruned before the tag protected it is an error, and the tag is removed.

`hearth note` records a fourth kind, `note`: a message and an optional author, the journal's commit messages. Notes are capped like run events, not thinned. Snapshots record what the machine was; notes record what someone did to it and why. `what-changed` reads them, and the MCP instructions ask agents to write one after any change they make or advise. Notes are capped at `state.max_note_bytes`. `state.thinned_kinds` takes only the state kinds, because thinning an event kind would lose events. `hearth bundle` includes the latest `bundle.recent_events` run events, and when notes were written but not their text: notes are free text, and an agent's note may quote a command line with a key.

`hearth log --all` lists every kind in one timeline. Ids begin with their zero-padded time, so sorting them sorts by time across kinds.

