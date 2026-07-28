# Alba — Cache / Incremental Design

Date: 2026-07-28 Status: approved

Sub-project 2 of the roadmap in
`.claude/superpowers/specs/2026-07-27-alba-core-design.md`.

## Overview

Alba's positioning is "task runner with intelligent caching". This
sub-project delivers that caching: fingerprinting of the `inputs` and
`outputs` declarations frozen in the core, skipping of unchanged beams,
and `.gitignore`-aware glob expansion. The semantics are skip-only, in
the spirit of go-task: a beam whose fingerprint is unchanged since its
last successful run is reported as `cached` and not executed. Outputs
are never archived or restored; that stays out of scope.

## Goals

- Skip a beam when its inputs, command, environment, arguments, and
  dependency fingerprints are all unchanged and its declared outputs
  exist on disk.
- Replay the logs of the last successful run on a cache hit, so a
  cached run stays readable and debuggable, in CI included.
- Expand `inputs` globs while ignoring what git ignores, using the
  `ignore` crate. The `outputs` existence check deliberately queries the
  disk raw: outputs typically live in git-ignored directories
  (`target/`, `dist/`), so filtering them through `.gitignore` would
  make their patterns never match.
- Give the cache a CLI surface: `--force` on `alba run` and
  `alba cache clean`.
- Keep the cache invisible when it cannot help: a beam without declared
  `inputs` always runs, and no cache failure may ever fail a run.

## Non-goals

- Store and restore of outputs (Turborepo-style archiving). The
  manifest format is versioned so this can be added later without
  migrations, but nothing is built for it now.
- Remote or shared caches.
- Git variables in the DSL (`git.branch` and friends). The roadmap
  placed them here, but they are orthogonal to caching and move to
  sub-project 7 (affected + git hooks), their natural habitat.
- Cross-process locking. Concurrent `alba run` invocations in the same
  project are tolerated, not coordinated.

## Key decisions

| Topic            | Decision                                                                 |
|------------------|--------------------------------------------------------------------------|
| Semantics        | Skip-only; no output archiving or restoration                            |
| Placement        | `cache` module inside `alba-engine`, as the core design planned          |
| Glob expansion   | In `alba-core`, `.gitignore`-aware via the `ignore` crate                |
| Hashing          | blake3 over file contents                                                |
| State location   | `.alba/cache/` at the project root, git-ignored by convention            |
| Cache writes     | Only a plain success writes; failures and `failed (allowed)` never do    |
| Cached logs      | Stored on success, replayed on a hit with a `replayed` marker            |
| Cacheability     | A beam without declared `inputs` is never cached                         |

## Architecture

A single `cache` module in `alba-engine` with three parts:

- **Fingerprint**: computes the hash of a beam that is ready to run.
- **Store**: reads and writes the persisted state under `.alba/cache/`.
- **Decision**: decides skip or run from the fingerprint, the manifest,
  and the presence of declared outputs.

Scheduler flow: when a beam becomes ready (all its `needs` finished),
the engine computes its fingerprint and decides. On a hit it emits a
`Cached` status, replays the stored logs through the existing event
channel, and releases dependents without executing anything. On a miss
it executes normally, then writes the manifest and logs if, and only
if, the beam succeeded. Failures and cancellations write nothing and
leave the previous manifest intact.

Glob expansion lives in `alba-core` because it is model resolution that
other consumers will reuse; the engine consumes the expanded, sorted
file lists.

Dependency rules are unchanged: `cli → engine → core → syntax`, and the
engine still sees executors only through the `Executor` trait. The
cache never talks to executors; it sits entirely in the scheduling
layer.

## Fingerprint recipe

One blake3 hash per beam, fed with:

- Each file resolved by the `inputs` globs, as a sorted list of
  (project-relative path, blake3 content hash) pairs.
- The rendered `run` command(s), after interpolation.
- The resolved `cwd` the commands run in: the same command in another
  directory is another invocation.
- The resolved `env` block.
- The beam arguments, when the beam is parameterized.
- The fingerprint of each `need`, in a stable order.

For a non-cacheable `need` (one without declared `inputs`), its
contribution is its static part only: rendered command, working
directory, resolved environment, and arguments. A non-cacheable
dependency therefore does not poison the cascade; if its actual output
changes, the dependent's own `inputs` catch the change by content.

The fingerprint recipe is part of the manifest format version: any
change to the recipe bumps the version and invalidates existing
manifests.

## Store format

Per beam, under `.alba/cache/`:

- A JSON manifest: format version, the fingerprint of the last
  successful run, the rendered `outputs` patterns to check, and the
  original run duration.
- A log file: the stdout and stderr lines of the last successful run,
  in emission order, with enough structure to replay them through the
  event channel.

Manifest writes are atomic (temporary file + rename). The `.alba/`
directory is created on demand. Alba never edits the user's
`.gitignore`; the documentation says to ignore `.alba/`, and Alba's own
repository leads by example.

## Decision rules

A beam is skipped if and only if:

1. It declares `inputs`.
2. A manifest exists, with the current format version.
3. The computed fingerprint equals the manifest's fingerprint.
4. Every declared `outputs` pattern is satisfied on disk: a literal
   path must exist; a glob pattern must match at least one file.

Anything else is a miss and the beam runs. `--force` skips the read
side entirely: everything runs, and successes rewrite their manifests.

## CLI surface and user experience

- A cache hit renders as a status line, for example
  `codegen │ cached — replaying last output`, followed by the replayed
  log lines with the usual beam-name prefix. The summary counts hits
  separately: `2 ran, 3 cached, 0 failed`. A cached beam's displayed
  duration is its original run duration, marked as such.
- `--force` on `alba run`: ignore the cache when reading, execute
  everything, rewrite manifests on success. The universal escape hatch.
- `alba cache clean`: removes `.alba/cache/`. The `cache` subcommand
  leaves room for a future `alba cache status`, but only `clean` ships
  now.
- JSON events (`--log-format json`): a new event type for the cached
  status, and replayed lines carry `replayed: true` so CI tooling can
  tell them from a real execution.
- Unchanged: exit codes, `alba check` (which never touches the cache),
  `--keep-going`, `allow_failure` semantics. A `failed (allowed)` beam
  writes no manifest; only a plain success is cacheable.

## Error handling and edge cases

- Corrupted or unreadable manifest: treated as a miss, silently
  rewritten on the next success. The cache must never fail a run; at
  worst it causes a re-execution.
- Unreadable input file (permissions, or deleted between glob
  resolution and hashing): the beam is treated as non-cacheable for
  this run, with a warning. Same philosophy: never fail because of the
  cache.
- `inputs` globs matching nothing: the beam is not cacheable for this
  run, with a warning. The fingerprint of an empty file list is a
  constant, so caching on it writes a manifest nothing can ever
  invalidate, and every later run replays it however much the sources
  changed. The ordinary causes are all mistakes worth naming: a
  misspelled path, an input directory `.gitignore` excludes, a pattern
  that does not compile. That last one is caught earlier too: `inputs`
  and `outputs` patterns are compiled when the Beamfile loads, so
  `alba check` reports a broken pattern instead of leaving it to match
  nothing.
- Concurrent runs in the same project: no lock; atomic manifest writes
  mean the worst case is a manifest overwritten by the last winner,
  hence one superfluous re-execution later. A real lock would be
  over-engineering here.

## Testing strategy

TDD (red, green, refactor) throughout, continuing the core's habits.

- `alba-core`: glob expansion tests. `.gitignore` respected, literal
  versus starred patterns, stable sorted results (the fingerprint must
  be deterministic).
- `alba-engine`: the bulk of the work, against the existing
  `FakeExecutor`. One test per invalidation cause (input content,
  rendered command, environment, arguments, `need` fingerprint); the
  skip when nothing changed; re-execution when an output is missing;
  the cascade (a dependency re-run for a real change invalidates its
  dependents; a non-cacheable `need` leaves dependents cacheable); a
  failure writing nothing; `--force`; the log replay events.
- `alba-cli`: end-to-end with `assert_cmd`. Two consecutive
  `alba run` invocations (the second fully cached, logs replayed);
  touching one file re-runs only the affected beams; `alba cache
  clean`; the `replayed` field in the JSON output.
- Performance guard: the existing guard extends to fingerprint
  computation. The cache overhead on a fully cached run must stay
  imperceptible, on the order of ten milliseconds on a typical
  Beamfile, hashing included.

## Success criteria

1. On the Alba repository itself, running `alba run build` twice in a
   row: the second run is fully `cached` and near-instant. The
   repository's `Beamfile` declares real `inputs` and `outputs`
   (dogfooding).
2. Touching a single source file re-runs only the beams actually
   affected by it.
3. CI green on macOS, Linux, and Windows.
