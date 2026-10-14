# ADR-006: `hearth watch`, the machine watched without anyone watching

- Status: **Accepted**
- Date: 2026-10-15

## Context

`doctor`, `ports` and `diff` answer questions when someone asks them. The failures in the user's notes happened when nobody was asking:
- a descriptor leak overnight;
- a runtime that wedged while its client retried;
- memory pressure building until jetsam or the watchdog acted.

By the time anyone looked, the evidence was gone, and the journal (ADR-003) held only the runs someone happened to make.

## Decision

`hearth watch [--for-seconds N]` runs `doctor` every `watch.doctor_interval_ms` and `ports` every `watch.ports_interval_ms`. Each round is exactly the command:
- measured the same way;
- recorded in the journal, so `log` and `diff` cover the whole time watched;
- with descriptor and thread trends measured between consecutive doctor rounds, the way `doctor --watch` measures between its two samples.

### What is reported

A problem is reported once when it appears and once when it clears, never on every round it lasts. Both the appearance and the clearing are printed (JSON lines with `--json`) and sent through `[notify]` (ADR-005).

- **Memory pressure** the kernel declares, warn or critical, reported at once. hearth invents no memory threshold. Warn rising to critical is a new problem, and the warn clears.
- **A resource reaching its limit.** A resource is reported if, at the growth rate measured since the last round, it would reach its limit within `watch.limit_horizon_ms`, for `watch.confirm_rounds` rounds in a row. It clears after missing as many rounds. One rate measured over one interval wobbles, and without the confirmation a count hovering near the horizon would open and clear on alternate rounds. The limit is the kernel-wide open-file limit, or a process's own open-file or thread limit. Growth alone is not reported: descriptor and thread counts move all the time, and a report on every uptick would be noise that trains people to ignore the channel.
- **A model runtime that vanished, restarted (new pid), stopped answering, or answered again**, between two ports rounds. A port counts as a runtime's if a runtime answered there in any round of this watch, so one that went silent and then vanished is still reported. Other listeners come and go, such as dev servers and browsers, and are left out. These are events, so each is reported once.
- **A round that failed.** For example, the journal could not be written, or the process table could not be read. This is a problem like the others, reported once. The watch goes on, and stops only when its output closes.

Problems are keyed by what they are about: `memory_pressure:<level>`, `files_growing`, or `<resource>:<pid>`. A rate that changes is the same problem, not a new one.

### Intervals and retention

The intervals and the horizon are `[watch]` keys, validated non-zero and representable by the clock. The journal keeps `state.max_snapshots_per_kind` per kind. How far back `diff` can reach is therefore that count times the interval: with the defaults, about 33 minutes of doctor rounds and about 3 hours of ports rounds. Raising retention is a configuration change.

## Consequences

- On the development machine, the first 22-second watch:
  - reported kernel memory pressure **warn**, with 4 GiB available beside a 35 GiB llama-server;
  - reported a remote-desktop host growing its open files at 3.2/s, 52 minutes from its limit, then cleared when the rate fell;
  - reported a test runtime as vanished within one ports round of its death.
- `watch` is the daemon's observation loop without the daemon. When `hearthd` exists it will run the same rounds, and `watch` stays as the way to run them by hand.
- A watch that measures nothing new costs one doctor sample per interval (proc and sysctl reads) and one ports round (one HTTP GET per listener, with learned deadlines).

## Addendum (2026-10-15): servers configured past the device

`ports` checks every llama.cpp server it finds, with the same computation as `hearth fit --port`. That uses the server's `/props` context times its slots, and the cache types on its command line. A server whose model at that context exceeds the device ceiling is a **warn** finding. The remediation gives the largest context that fits and the cache types that would keep the current one.

`watch` (and hearthd) report warn findings from `ports` as problems, once when they appear and once when they clear, keyed by finding code and pid. Silent ports are left out, because they are already reported as runtime changes.

On the development machine, the first watch round flagged the user's llama-server: 262,144 tokens of q8_0 cache need 56.9 GiB against Metal's 51.8 GiB. This is the configuration behind the memory pressure every earlier `doctor` showed.

