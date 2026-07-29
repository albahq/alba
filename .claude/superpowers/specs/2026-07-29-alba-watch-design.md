# Alba — Watch Mode Design

Date: 2026-07-29 Status: approved

Sub-project 4 of the roadmap in
`.claude/superpowers/specs/2026-07-27-alba-core-design.md`.

## Overview

Watch mode keeps a run alive: `alba run --watch` executes the requested
beam, then waits for file changes and re-runs. The cache (sub-project 2)
does the incremental work — a triggered run goes through the normal
scheduler, so only the beams actually affected by a change execute and
the rest report `cached`. Watch is therefore a thin, well-behaved loop
around machinery that already exists: load, plan, run, wait, repeat.

What triggers a re-run is exactly what invalidates the cache: the
`inputs` declarations. There is no new DSL surface. The core design once
reserved a per-beam `watch` field; this design retires that idea — the
`inputs` globs are the single source of truth for "what this beam
depends on".

## Goals

- `alba run --watch [beam] [args]`: run, then re-run on changes to the
  files matched by the `inputs` of the requested subgraph.
- Restart semantics: a change arriving mid-run cancels the current run
  cleanly and starts a fresh one after a debounce. Latest code wins.
- Hot Beamfile reload: editing any loaded Beamfile (root or import)
  reloads the project, recomputes the subgraph and the watched set, and
  re-runs. A Beamfile that no longer parses shows its diagnostics and
  the session keeps watching until it parses again.
- First-class headless behaviour: watch events flow through the
  existing event channel, so `--log-format json` and the future TUI
  consume the same stream.

## Non-goals

- A per-beam `watch` DSL field. `inputs` is the trigger; a beam without
  `inputs` never triggers (but still re-executes when a triggered run
  reaches it, since the cache never skips it).
- A queue/finish-then-rerun strategy or a `--watch-strategy` flag.
  Cancel-and-restart is the only mode.
- Serving, process supervision, or long-running beam management
  (nodemon-style restarts of a server beam). A beam is still a task
  that ends.
- Polling fallback tuning, custom debounce flags, or per-beam debounce.
  One built-in debounce for everyone.

## Key decisions

| Topic             | Decision                                                              |
|-------------------|-----------------------------------------------------------------------|
| Trigger           | The `inputs` globs of the requested subgraph, plus loaded Beamfiles   |
| CLI               | `--watch` flag on `alba run`; no dedicated subcommand                 |
| Mid-run change    | Cancel the current run (existing token), debounce, restart            |
| Beamfile change   | Hot reload; broken Beamfile shows diagnostics and waits, never exits  |
| Placement         | `watch` module inside `alba-engine`, next to `cache`                  |
| File watching     | `notify` + `notify-debouncer-full`, behind an internal watcher trait  |
| Debounce          | ~200 ms, built in, not configurable                                   |
| `--force` + watch | Forces the initial run only; triggered runs use the cache normally    |
| Exit              | Ctrl-C ends the session with exit code 0                              |

## Architecture

A `watch` module in `alba-engine`, alongside `cache`. New workspace
dependencies: `notify` and `notify-debouncer-full`.

The loop lives in the engine and cycles through:

1. **Load** the project via `alba-core`. In watch mode the CLI hands
   the engine the Beamfile path and delegates loading, so the reload
   step belongs to the loop, not to the CLI.
2. **Resolve** the requested subgraph and compute the watched set: the
   `inputs` patterns of every beam in the subgraph, plus every loaded
   Beamfile (root and imports).
3. **Run** through the existing scheduler. The cache stays active, so
   only affected beams execute; the rest report `cached`.
4. **Wait** for debounced file-system changes, then filter: an event
   counts only if its path matches a subgraph `inputs` pattern (pattern
   matching, not a frozen file list, so files created after startup are
   caught) and is not git-ignored, or if it is a loaded Beamfile.
   `.git/` and `.alba/` are always excluded.
5. **Restart**: if a run is still in flight, cancel it through the
   existing `CancellationToken` (SIGTERM, grace, SIGKILL), then go back
   to step 3 — or to step 1 when the change touched a Beamfile, which
   recomputes the subgraph and the watched set.

Ctrl-C leaves the loop and ends the session.

The watcher sits behind a small internal trait — a stream of debounced
change batches — so engine tests inject synthetic events without
depending on file-system timing. `notify` provides the real
implementation.

Dependency rules are unchanged: `cli → engine → core → syntax`; the
engine already depends on `alba-core` and keeps seeing executors only
through the `Executor` trait. The event channel remains the only
contract between execution and display: it gains watch variants
(session started, waiting for changes, run triggered with the
responsible paths, run interrupted by a new change), consumed alike by
the headless renderer, `--log-format json`, and the future TUI.

## CLI surface and user experience

- `alba run --watch [beam] [args]`: composes with `--jobs`, `--output`,
  `--log-format json`, beam arguments, and the default beam. Arguments
  are frozen for the session; every triggered run reuses them.
- Between runs, a status line: `watching — 42 files, waiting for
  changes`. When a change triggers, a header names the responsible
  paths (truncated beyond a few), then the run renders as usual.
- On a TTY, the screen is cleared at the start of each triggered run
  (a clean visual anchor, watchexec-style). Off-TTY and in
  `--log-format json`, never: runs follow each other, separated by the
  watch events.
- Debounce of ~200 ms: a burst of saves (a formatter, `git checkout`)
  produces a single run.
- `--force --watch` forces the initial run only; triggered runs go
  through the cache normally, otherwise every keystroke would re-run
  the whole subgraph.
- Broken Beamfile at startup: diagnostics and exit code 2, exactly like
  `alba run`. Broken during the session: diagnostics render, nothing
  executes, the session keeps watching and resumes as soon as the file
  parses again.
- Exit: Ctrl-C ends the session with exit code 0 — individual run
  failures were already reported as they happened. Exit code 2 stays
  reserved for Alba errors at startup (unparseable Beamfile, watcher
  unavailable).
- If no beam in the subgraph declares `inputs`, an explicit startup
  warning says the session will only watch Beamfiles; hot reload lets
  the user add `inputs` without restarting.

## Error handling and edge cases

- **Feedback loops** (a beam writes into a path its subgraph's `inputs`
  match): the git-aware filter covers the typical case — `target/`,
  `dist/` are git-ignored, their writes trigger nothing. A beam writing
  to a git-tracked file matched by `inputs` can loop; that is
  documented as a Beamfile mistake, and the debounce at least prevents
  a tight spin.
- **Watcher unavailable at startup** (inotify limits, permissions):
  clear error, exit code 2. Mid-session, a `notify` queue overflow
  (rescan) is treated as "something changed" and triggers a run — the
  cache absorbs the imprecision; at worst a few more beams report
  `cached`.
- **Editor atomic saves** (write to a temporary file, then rename):
  handled by `notify-debouncer-full`, which stitches renames back
  together; the debounce coalesces the rest.
- **Run cancelled by a new change**: cancelled beams write nothing to
  the cache (existing behaviour), so the next run picks up exactly the
  work that is needed. The interrupted run's summary is marked as
  interrupted by the watch, not as a failure.
- **Deletion of a watched file**: a change like any other — the run
  triggers, the fingerprint changes, the beam re-executes (or fails
  honestly if its command needed the file).
- **Parameterized beams in the subgraph**: nothing special; arguments
  are frozen for the session and already feed the fingerprint.

## Testing strategy

TDD (red, green, refactor) throughout, continuing the previous
sub-projects' habits.

- `alba-engine` (the bulk): the loop tested with the existing
  `FakeExecutor` and the injected watcher trait — a change re-runs only
  the affected beams (the cache skips the rest); cancel-and-restart
  when a change lands mid-run; debounce coalescing; Beamfile hot reload
  (including broken then repaired); git-ignored paths and `.alba/`
  never trigger; the watch event variants (order and content).
- `alba-executors` / `alba-shell`: untouched; watch never reaches them.
- `alba-cli`: end-to-end with `assert_cmd` on a real long-running
  process: start, wait for the status line, modify a file, assert the
  re-run, terminate cleanly. `--log-format json` is the deterministic
  oracle. One test uses the real `notify` watcher (not the injected
  trait) to validate the system integration on all three platforms.
- Performance guard: the latency between the file event and the start
  of the run (debounce excluded) must stay imperceptible, on the order
  of a few tens of milliseconds.

## Success criteria

1. Dogfooding: `alba run --watch test` on the Alba repository itself —
   touching a `.rs` file re-runs `test` (and only the affected beams).
2. Editing the Beamfile mid-session reloads and re-runs without a
   restart.
3. CI green on macOS, Linux, and Windows.
