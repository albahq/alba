# Alba — Affected Runs, Git Variables, and Git Hooks

Date: 2026-08-25 Status: approved

## Overview

Sub-project 7 of the [core design](2026-07-27-alba-core-design.md), the last on its roadmap: the git integration.
Three pieces, each small, sharing one module that talks to git:

- `alba run --affected <ref>` runs only the beams touched by what changed since `<ref>`, computed from the
  `inputs` globs the cache and watch mode already rely on.
- A `git` object in the DSL (`git.branch`, `git.sha`, `git.short_sha`, `git.dirty`), moved here from the cache
  sub-project which declared them out of its scope.
- Git hooks declared in the Beamfile (`hook pre-commit { beam check }`), installed with `alba hooks install`, and
  run through `alba hook <name>`: the lefthook/husky idea, with the Beamfile as the single source of truth.

No new crate and no new dependency: git is driven through its CLI, the same decision the docker executor made.

## Goals

- Select beams by git diff: direct matches on `inputs` and Beamfile changes, closed over dependents, with and
  without a target beam, listable without running, and compatible with `--watch`.
- Expose the current branch, commit, and dirtiness to Beamfile expressions.
- Declare hooks in the Beamfile, validated by `alba check` against git's known hook names and argument counts.
- Install hooks once per clone through `core.hooksPath`; adding a hook declaration afterwards needs no reinstall.
- A hook run is exactly `alba run <beam> <git args>` with the text renderers, so anything a hook does can be
  reproduced by hand.

## Non-goals

- Passing arguments to dependencies (`needs [deploy("staging")]`). A parameterized beam still cannot be a
  dependency; hooks only ever bind git's arguments to the target beam, which the scheduler already supports.
- Forwarding git's stdin to hook beams (the ref list `pre-push` receives). Documented as a limit.
- `git.tag`, `git.root`, or any git variable beyond the four listed.
- A JSON output for `alba hooks install|uninstall`.
- Driving git through a library (`gix`, `git2`).

## Key decisions

| Topic              | Decision                                                                                      |
|--------------------|-----------------------------------------------------------------------------------------------|
| Git access         | Shell out to the `git` CLI from one module, `alba-core/src/git.rs`; no library dependency     |
| Changed files      | `git diff --name-only <ref>` ∪ `git ls-files --others --exclude-standard`, relative to the project root |
| Affected beams     | `inputs` pattern match or declaring Beamfile changed, closed over dependents; no `inputs` → never directly affected |
| Selection forms    | `--affected <ref> <beam>` filters the beam's subgraph; `--affected <ref>` alone targets every affected beam |
| Scheduler contract | `plan` takes a set of targets instead of one; needs of a target run as usual (the cache skips them) |
| Watch              | `--affected` composes with `--watch`: the full subgraph is watched, targets are recomputed per run |
| Git variables      | `git.branch`, `git.sha`, `git.short_sha`, `git.dirty`; evaluated at load, once per load; error only when accessed |
| Hook declaration   | `hook <git-name> { beam <target> }`; root Beamfile only; name and arity validated at load       |
| Hook arguments     | Git's arguments become the target beam's positional parameters, truncated to what it declares  |
| Installation       | `core.hooksPath = .alba/hooks`; one identical script per known git hook; undeclared hook exits 0 |
| Hook run           | `alba hook <name> [args]` ≡ `alba run <beam> [args]` with `--no-ui`, grouped output, run exit code |

## DSL changes

### The `git` object

| Expression      | Value                                                                                   |
|-----------------|-----------------------------------------------------------------------------------------|
| `git.branch`    | Current branch name; `HEAD` when detached                                               |
| `git.sha`       | Full SHA of `HEAD`                                                                      |
| `git.short_sha` | Abbreviated SHA (`git rev-parse --short HEAD`)                                          |
| `git.dirty`     | `true` when the working tree or the index differs from `HEAD`, untracked files included |

- Usable wherever an expression is: `let`, `run`, `description`, `env`, `inputs`, `outputs`, `cwd`, executor
  options. `sha`, `short_sha`, and `branch` are strings; `dirty` is a boolean, so
  `{if git.dirty then '-dirty' else ''}` works with the existing expression forms.
- Evaluated while the Beamfile loads, lazily on first access, and cached for the rest of the load: a single
  `head()` call answers all four fields.
- Outside a git repository, in a repository with no commit yet, or with `git` missing from the PATH: a load error
  pointing at the expression that accessed `git`, raised only when an expression does. A Beamfile that never
  reads `git` never runs `git`.
- `git` becomes a reserved name: `let git = ...` is a syntax error.

### The `hook` declaration

```
hook pre-commit { beam check }
hook commit-msg { beam check_message }
```

- `<name>` must be one of git's hook names. `alba-core` carries the fixed list with, for each hook, the number of
  arguments git passes to it (`pre-commit` 0, `commit-msg` 1, `pre-push` 2, `post-checkout` 3, ...). An unknown
  name is a load error.
- `beam` is the only field and is required: a local or namespaced (`api:check`) beam name. An unknown beam is a
  load error, reported like an unknown `needs`.
- The target beam may declare at most as many parameters as the hook provides arguments; more is a load error.
  Fewer is fine: git's extra arguments are dropped.
- The same hook declared twice is a load error. `hook` declarations in imported Beamfiles are ignored, exactly as
  `default` is: only the root decides.
- `Project` gains `hooks: Vec<Hook>` (`name`, `beam`, source span).
- `alba check` counts them (`✓ Beamfile: 5 beams, 2 hooks`) and warns when hooks are declared but
  `core.hooksPath` is not `.alba/hooks`: `⚠ 2 hooks declared, run 'alba hooks install'`.

A hook that wants several beams points at an aggregating beam. That beam is what `alba run` would run by hand,
which keeps every hook reproducible from the command line:

```
beam check_message(path) {
  needs [fmt, lint]
  run "commitlint --edit {path}"
}
```

## Affected selection

### Changed files

`alba-core/src/git.rs::changed_files(ref)` returns the union of:

- `git diff --name-only <ref>`: committed, staged, modified, deleted, and renamed paths since `<ref>`, working tree
  included;
- `git ls-files --others --exclude-standard`: untracked paths, `.gitignore` respected.

Paths come back relative to the repository root and are rebased onto the project root (the directory of the root
Beamfile). A Beamfile in a subdirectory of a monorepo sees only what falls under it; a path outside the project
matches no glob. An invalid `<ref>` surfaces git's own error.

### Affected beams

`alba-engine/src/affected.rs`, a pure function over the project graph and a list of changed paths, testable
without git:

1. A beam is directly affected when one of its `inputs` globs matches a changed path (pattern matching on the
   path, as watch mode does, not membership in the resolved file list, so a deleted file counts) or when the
   Beamfile declaring it changed.
2. The set is closed over dependents: every beam that transitively `needs` an affected beam is affected.
3. A beam with no `inputs` is never directly affected. It runs only through rule 2, or as the explicit target.

### Commands

- `alba run --affected <ref> <beam> [args]`: the targets are the affected beams inside `<beam>`'s subgraph. When
  `<beam>` itself is not affected, neither is anything in its subgraph (rule 2), so the run is empty.
- `alba run --affected <ref>`: the targets are every affected beam in the project. An affected beam that declares
  parameters is skipped (nothing can bind its arguments); `alba affected` still lists it, marked
  `(takes parameters)`. The others run.
- The scheduler receives a set of targets instead of one: `plan(project, targets: &[BeamId], params)`. The needs
  of a target run as they always have (the cache skips the unchanged ones); a beam that is neither a target nor a
  need of one is absent from the plan. This is the only change to the scheduler's contract, and the single-target
  callers pass a one-element set.
- An empty run prints `✓ nothing affected by <ref>`, exits `0`, and emits no beam event.
- `alba affected <ref>`: lists the affected beams without running, one per line, or `{"beams": [...]}` under
  `--log-format json`. Exits `0` even when the list is empty. The dry run of `--affected`, meant for CI.
- The run's JSON stream gains two fields on the run-started event: `targets` (the beams asked for; empty when an
  affected run found nothing) replaces the single-beam `target`, and `affected_by` (the git reference) is present
  only for an affected run, omitted otherwise. Nothing else in the stream changes.

### With `--watch`

`--affected <ref> --watch` composes: the reference is fixed, the working tree moves.

- The watched set is the full subgraph of `<beam>` (or the whole project without a beam), not the beams affected
  at startup: touching a file of a not-yet-affected beam triggers a run, which recomputes the set and brings the
  beam in.
- Every run, initial or triggered, recomputes the targets against `<ref>`. Reverting a change drops its beam out
  of the set.
- A recomputed empty run is an ordinary run for the session: the header reports it, the session goes back to
  waiting.

## Hooks at run time

### `alba hooks install`

1. Load the Beamfile (validation only) and locate the repository root (`git rev-parse --show-toplevel`).
2. If `core.hooksPath` is set and is not `.alba/hooks`: error, nothing touched
   (`core.hooksPath already points to <x>, remove it or uninstall that tool first`).
3. Write `.alba/hooks/<name>`, executable, for every hook git knows, all with the same content:

   ```sh
   #!/bin/sh
   command -v alba >/dev/null 2>&1 || { echo "alba: not found on PATH, install it or run 'alba hooks uninstall'" >&2; exit 1; }
   exec alba hook "$(basename "$0")" "$@"
   ```

   Git for Windows runs hooks through its bundled `sh`, so the same script serves every platform. A teammate
   without `alba` on the PATH gets a clear failure and a blocked commit rather than a silently skipped hook.
4. `git config core.hooksPath .alba/hooks` (repository-local). Idempotent: running it again rewrites the scripts
   without error. `.alba/` is already git-ignored by convention.

Installing scripts for every hook, not only the declared ones, is what makes a later `hook` declaration work
without a reinstall: the only install that matters is the first one per clone.

### `alba hooks uninstall`

Unsets `core.hooksPath` only when it is `.alba/hooks`, then removes the directory. Otherwise an error, nothing
touched.

### `alba hook <name> [args]`

The entry point the scripts call, usable by hand for debugging.

- No Beamfile, or no `hook <name>` declared: exit `0`, silent. This is what makes the blanket install harmless.
- Beamfile present but invalid: the diagnostic on stderr, exit `2` (the binary's Alba-error code; git blocks on
  any non-zero exit), the git operation blocked. A broken Beamfile must not let a hook through silently.
- Otherwise the strict equivalent of `alba run <beam> <args...>` with the text renderers forced (`--no-ui`),
  `grouped` output, the cache active, default `--jobs`. The exit code is the run's, so git refuses the operation
  when the beam fails.
- Git's arguments are truncated to the number of parameters the target beam declares.
- `--file` is honored as on every subcommand. The scripts pass none: git runs hooks from the repository root, so
  the expected Beamfile is the root one.

## Where it lives

- `alba-core/src/git.rs`: the only place that spawns `git`. `repository_root()`, `head()` (branch, SHA, dirty in
  one pass), `changed_files(ref)`, `hooks_path()` / `set_hooks_path()` / `unset_hooks_path()`. Parsed output,
  `GitError` carrying the command line and git's stderr.
- `alba-syntax`: `hook` in the lexer, parser, and AST.
- `alba-core`: `git` reserved and evaluated in `eval.rs` (lazily, cached per load); hook validation in
  `loader.rs` (known name, arity, duplicate, root only); `Project::hooks`.
- `alba-engine`: `affected.rs` and the multi-target `plan`.
- `alba-cli`: the `affected`, `hook`, and `hooks install|uninstall` subcommands, `--affected <ref>` on `run`,
  the warning in `check`.
- No new crate, no new dependency.

## Error handling

- `git` missing from the PATH: a clear error (`git not found on PATH`) at the first invocation, raised only when a
  git feature is used. A project that reads no `git.*`, declares no hook, and runs without `--affected` never
  spawns `git`. Outside a repository: the same logic, with git's own message.
- Load diagnostics (unknown hook name, arity, duplicate hook, unknown target beam, `let git`) follow the existing
  Rust-style format with the source location.
- `--affected` with an invalid reference: git's error, exit as for any load-time failure.

## Testing strategy

- `alba-syntax`: insta snapshots of the `hook` AST and of its error messages.
- `alba-core`: `git.rs` against a real temporary repository (`git init` in a tempdir, a few commits, a branch,
  untracked and deleted files); hook validation and `git.*` evaluation against the same repository, plus the
  outside-a-repository case.
- `alba-engine`: `affected.rs` with fabricated path lists and no git: closure over dependents, a beam without
  `inputs`, a changed Beamfile, a deleted file, an unaffected target; the multi-target scheduler with the
  existing `FakeExecutor`.
- `alba-cli`: end to end with `assert_cmd` on a temporary repository: `alba affected`, `alba run --affected` with
  and without a beam, the empty run, `hooks install` followed by a real `git commit` that triggers the hook and
  a `git commit` blocked by a failing beam, `uninstall`, and `--affected --watch` recomputing after a change.
- Performance guard: the affected computation on a typical project stays on the order of ten milliseconds,
  `git`'s own time excluded.

## Success criteria

1. Dogfooding on the Alba repository: `hook pre-commit { beam fmt }` and `hook pre-push { beam check }` in the
   Beamfile, installed and used by the maintainers.
2. On a branch off `main`, `alba run --affected main check` reruns only the beams whose inputs moved.
3. CI green on macOS, Linux, and Windows.
