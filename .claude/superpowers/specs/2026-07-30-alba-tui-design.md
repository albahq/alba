# Alba — TUI

Date: 2026-07-30 Status: approved

## Overview

Sub-project 5 of the [core design](2026-07-27-alba-core-design.md): the interactive terminal interface. The TUI
becomes the default experience for `alba run` on a terminal — beam tree, per-beam logs, live statuses, and a
progress bar — while the existing headless renderers remain the experience for CI, pipes, and anyone who asks
for text explicitly.

The TUI is fully interactive: it can rerun a beam, cancel the run, toggle watch, search logs, select and copy
log text, and show the dependency graph. That interactivity is what shapes the architecture: the engine gains a
long-lived, commandable **session** that generalizes the watch loop, and the TUI is a thin consumer/producer on
the two channels — events down, commands up. The dependency direction does not change.

## Goals

- TUI by default on a TTY; headless everywhere else, unchanged.
- Full interactivity: rerun a beam (optionally bypassing the cache), cancel the run, toggle watch.
- Comfortable log reading: follow mode, scrolling, incremental search, pane-aware selection and copy.
- A read-only dependency graph view that updates live.
- A session abstraction in the engine that both the TUI and headless watch share.
- The terminal is always restored, and quitting leaves a useful text trace in the scrollback.

## Non-goals

- A daemon or client/server split (still a core non-goal; the session lives in the single process).
- TUI for `alba check`, `alba cache`, or the bare-`alba` beam listing — those stay plain text.
- Actions from the graph view (it is read-only; actions live in the tree).
- Configurable keymaps or themes (revisit on demand).
- Cross-run log history (a beam's buffer resets when the beam reruns).

## Key decisions

| Topic            | Decision                                                                                    |
|------------------|---------------------------------------------------------------------------------------------|
| Activation       | Default on a TTY; headless when not a TTY, `--log-format json`, explicit `--output`, or `--no-ui`; `--ui` forces |
| Orchestration    | `Session` in `alba-engine`, generalizing the watch loop; commands up, events down            |
| Event contract   | Extend `RunEvent` (precedent: watch events) with `RunStarted` and `ProjectBroken`            |
| Crate            | `alba-tui` (ratatui + crossterm); chain `cli → tui → engine → core → syntax`                 |
| Layout           | Left: beam tree with statuses/durations; right: selected beam's logs; header progress bar    |
| Graph view       | Dedicated read-only screen (`g`), layered topological ASCII DAG, live statuses               |
| Copy             | Built-in copy mode (keyboard and mouse), OSC 52 with `arboard` fallback                      |
| Log memory       | Ring buffer per beam, 10 000 lines, truncation announced; reset when the beam reruns         |
| End of run       | The TUI stays open; `q` quits and replays the summary and failed beams' logs                 |
| Exit codes       | `q` → last run's code if it ran to completion, 130 otherwise; Ctrl-C when idle → 130; startup error → 2 |

## Architecture

Two pieces, one per layer.

### The `Session` (alba-engine)

A `Session` owns the durable state a run does not: the loaded project (reloaded when the Beamfile changes), the
notify watcher, and the current run if one is underway. It generalizes the existing watch loop: a filesystem
change and a `RunBeam` command from the TUI converge on the same "trigger a run" point.

Two channels:

- **Commands in** (mpsc of `SessionCommand`): `RunBeam { id, force }`, `CancelRun`, `SetWatch(bool)`,
  `Shutdown`.
- **Events out**: the existing `RunEvent` stream, extended (see the contract below).

Headless `alba run --watch` becomes a session nobody sends commands to — one orchestration code path for both
worlds.

### The `alba-tui` crate

The crate the core design reserved, on ratatui + crossterm. It mirrors the renderers: it consumes the event
stream, sends commands, and knows nothing about how a beam executes. The dependency chain stays strictly
downward: `alba-cli → alba-tui → alba-engine → alba-core → alba-syntax`.

### Activation

The TUI is the default when stdout is a TTY. It yields to headless when any of these hold:

- stdout is not a TTY (CI, pipes);
- `--log-format json`;
- an explicit `--output` (asking for a text layout is asking for text);
- the new `--no-ui` flag.

A symmetric `--ui` forces the TUI for edge cases. `alba check`, `alba cache`, and the beam listing are plain
text regardless.

### On exit

The terminal is restored and the last run's summary is replayed as text — the same summary the headless
renderers print — followed by the failed beams' logs from their buffers, so quitting leaves a scrollable trace
rather than an empty screen.

## Command / event contract

### Commands (TUI → session)

- `RunBeam { id, force }` — run the given beam and its subgraph; `force` bypasses the cache (the CLI's
  `--force`, scoped to one beam). If a run is underway it is cancelled first — the same rule watch applies to a
  file change today.
- `CancelRun` — cancel the current run through the existing cancellation token; the session stays alive.
- `SetWatch(bool)` — turn filesystem watching on or off without touching the current run.
- `Shutdown` — cancel any run, close the event stream, end the session.

### Events (session → TUI)

`RunEvent` is extended rather than wrapped — the precedent set by `WatchWaiting`/`WatchTriggered` — and the
JSON renderer carries the new events too, since the JSON stream is a public contract:

- `RunStarted { target, beams, edges }` — the explicit boundary of a run, with a snapshot of the subgraph:
  beam ids and dependency edges. This feeds the tree and the graph view, and is necessary regardless: the
  Beamfile can reload mid-session, so a run's graph is not known once and for all. Today this boundary is
  implicit (the first `BeamStarted`); making it an event gives headless renderers an anchor too.
- `ProjectBroken { diagnostic }` — the Beamfile no longer loads (the session parks, as watch does today): the
  rendered diagnostic arrives as an event and the TUI shows it in the log pane instead of losing it to stderr.

### Interrupts

`Ctrl-C` cancels the run if one is underway, otherwise quits. `q` always quits (cancelling whatever runs). No
double-Ctrl-C convention to memorize.

## Layout

```text
┌─ alba · run build ── ▰▰▰▰▰▰▱▱▱▱▱▱▱▱ 2/5 · 4.2s ─────────────────────────────┐
│ BEAMS                    │ logs · api:build                                 │
│                          │                                                  │
│ ✔ codegen          1.2s  │ Compiling proc-macro2 v1.0.86                    │
│ ⚡ api:codegen      0.8s  │ Compiling serde v1.0.210                         │
│ ▶ api:build        3.4s… │ Compiling api v0.1.0 (/repo/api)                 │
│ ○ build                  │ warning: unused import: `std::fmt`               │
│ ○ test                   │   --> src/lib.rs:4:5                             │
│                          │                                                  │
│ ✔ 1  ⚡ 1  ✖ 0  ○ 2      │ [/] search   [g] graph   [↑↓] scroll             │
└─ q quit · r rerun · c cancel · w watch ─────────────────────────────────────┘
```

- **Header**: progress bar (deterministic — the plan knows the subgraph size upfront; a cached beam counts as
  done), finished/total, and the run's elapsed time ticking live. In watch, between runs, the header switches
  to the waiting state (`waiting · 42 files watched`); the bar restarts on each triggered run.
- **Tree** (left): `✔` succeeded, `⚡` cached, `▶` running (duration ticking), `✖` failed, `○` pending; the
  pane footer counts beams per status.
- **Logs** (right): the selected beam's output; footer shows follow state and mode hints.
- **Bottom bar**: the always-available actions and the current mode.

## TUI internals

**Loop and state.** One `tokio::select!` loop over three sources: the session's `RunEvent`s, crossterm input,
and a clock tick. It updates a pure `AppState` — beams with status and duration, selection, scroll position,
current mode — and rendering is a function of that state. Redraws are event-driven but capped (~30 fps); the
tick keeps durations and the progress bar alive between events.

**Log memory.** A ring buffer per beam, capped at 10 000 lines; beyond that the oldest lines drop and a leading
`… N older lines truncated` line says so. A day-long watch session keeps bounded memory. A beam's buffer resets
when the beam reruns.

**Follow mode.** The log pane follows the tail by default. Scrolling up suspends following — read in peace
while the run continues — and `G` (or scrolling back to the bottom) resumes it. The pane footer shows
`● following` or `↑ paused`.

**Modes**, shown in the bottom bar:

- **Normal** — tree navigation (`↑↓`/`jk`), log scrolling, actions: `r` rerun, `f` rerun bypassing the cache,
  `c` cancel, `w` toggle watch.
- **Search** (`/`) — incremental over the selected beam's logs, matches highlighted, `n`/`N` to cycle, `Esc`
  to leave.
- **Copy** (`v`, or click-drag in the log pane) — keyboard selection (movements extend with `v`) or mouse
  selection; `y` copies through OSC 52, falling back to the system clipboard (`arboard`) when the terminal
  does not support it; then back to Normal. Pane-aware: the selection covers log text only, never the tree
  sitting beside it.
- **Graph** (`g`) — the run's DAG in topological layers, edges drawn, statuses colored and updating live;
  navigate between nodes, `Enter` selects that beam and returns to the main view, `Esc` returns without
  changing the selection.

**Help** (`?`): the full keymap as an overlay. Resizing is handled natively by ratatui; below a floor
(roughly 40×10) the TUI shows "terminal too small" rather than drawing wrong.

## Error handling

**Restore the terminal, always.** The alternate screen and raw mode are held by an RAII guard, doubled by a
panic hook: if the TUI panics, the terminal is restored *before* the panic message prints. This is the first
thing the crate's skeleton establishes.

**Beamfile broken at startup.** No TUI: the diagnostic prints to stderr and the process exits 2, as today. The
TUI only opens on a project that loads.

**Beamfile broken mid-session** (a failed reload): the session parks — exactly the current watch behavior —
and `ProjectBroken` brings the diagnostic into the log pane, the header switching to
`parked · waiting for a valid Beamfile`. When the file loads again the session resumes and announces it.

**A failed beam** is not exceptional: `✖` in the tree, logs in place, summary in the pane footer, and the TUI
stays open — that is precisely when it is useful.

**Exit codes.** The TUI observes; it does not change the 0/1/2/130 vocabulary:

- `q` — the deliberate exit: the code of the session's **last run, provided it ran to completion** (0 or 1),
  so `alba run build && ship` keeps its meaning when build ran under the TUI. In every other case — no run
  yet, a parked session, or a last run that was cancelled (by `c`, Ctrl-C, or a watch trigger) — 130: an
  older green run must not vouch for sources that changed since.
- `Ctrl-C` when nothing runs — quits with 130, consistent with the headless interruption.
- An Alba error at startup — 2, before the TUI ever opens.

## Testing strategy

TDD throughout, split along the architecture's seams:

- **`Session` (alba-engine)** — driven with the existing `FakeExecutor`: inject commands, assert event
  sequences. Key cases: `RunBeam` during a run cancels then restarts; `CancelRun` leaves the session alive;
  `SetWatch` toggles without killing the current run; `Shutdown` closes the stream cleanly; a failed reload
  emits `ProjectBroken` and the session resumes when the file heals. The watch loop's test harness carries
  over.
- **New events** — `RunStarted` (with its graph snapshot) and `ProjectBroken` go through the JSON renderer:
  insta snapshots updated, since the JSON stream is a public contract.
- **`AppState` (alba-tui)** — pure state, the most testable part: feed synthetic `RunEvent` sequences, assert
  the outcome. Statuses and durations, ring-buffer truncation at 10 000 lines, follow suspension and
  resumption, search matches, copy-mode selection, buffer reset on rerun.
- **Rendering** — ratatui's `TestBackend` draws into an in-memory buffer: insta snapshots of the key screens
  (main view mid-run, with a failure, parked session, graph view, help, terminal too small). Diagnostics have
  been snapshot-tested as a product feature since `alba-syntax`; the TUI's screens are tested in the same
  spirit.
- **Keyboard** — the key → state-transition/command table, unit tested (including Ctrl-C's cancel-or-quit
  rule).
- **End to end (alba-cli)** — activation rules under `assert_cmd` (no TTY → headless, `--log-format json` →
  never a TUI), and a small smoke suite under a pseudo-terminal (`portable-pty`): launch `alba run` in a PTY,
  assert the alternate screen is entered, send `q`, assert restoration, the summary replay, and the exit
  code. Deliberately small — confidence comes from the layers below; the PTY only verifies the real plumbing.
- **Copy** — the OSC 52 emission is asserted by capturing the escape sequence; the `arboard` fallback stays
  manually verified (no clipboard in CI).

## Success criteria

- The TUI is the daily driver for `alba run` on the author's own projects, watch sessions included.
- Headless behavior is byte-for-byte unchanged in CI and pipes; the existing watch tests keep passing on top
  of the `Session`.
- The terminal is restored on every exit path, panic included.
- Quitting leaves the summary and failing logs in the scrollback.
