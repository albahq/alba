# Alba: Incremental Beams on Git Changes

Date: 2026-11-26 Status: approved

Answers [albahq/alba#49](https://github.com/albahq/alba/issues/49).

## Overview

On a large codebase, quality tools are only affordable on what the branch changed: linters and formatters on the
changed files, a static analyser on the modules the branch touched. Alba already knows which beams a diff
affects (`--affected <ref>`), but a beam that runs learns nothing about what changed, so `phpcs` correctly
selected for one changed file still lints the whole tree. The diff is also taken from the tip of the reference,
not from the merge base, so commits that landed on the target branch after the branch was cut count as changes.

The workaround today is a `changes` beam on `system_shell` that recomputes the diff, writes one list per tool to
files, and beams that `cat` their list and declare it in `inputs`. This design moves all of that into Alba:

- The reference is declared once in the Beamfile, in a `git { base ... }` block, computed from the branch with a
  new `match` expression, and overridable from the command line.
- Diffs are always taken from the merge base with that reference.
- A beam reads its own changed files with `git.changes('glob')` in `run` or `env`, and reduces them to
  directories with `.dirs('glob')`, a generic list method.
- Selection and mode are separate. Which beams run is decided as today (`alba run X` runs X's subgraph, and
  `--affected` narrows it). What `git.changes()` returns is incremental by default once a `base` is declared;
  `--no-affected` asks for full mode.

## Goals

- A beam can run only on the files changed since a reference, with the diff taken from the merge base.
- The target branch rule (`hotfix/*` against `master`, the rest against `develop`) lives in the Beamfile, in
  Alba's sight, not in shell.
- Changed files can be reduced to whatever a project calls a module, without Alba knowing the project's layout.
- No `system_shell`, no intermediate file, no extra `inputs` entry is needed for any of the above.
- A daily `alba run phpcs` lints only the changed files without any flag.
- Beams that do not call `git.changes()` (`deploy`, `clean`, hook beams, the `default` beam) run exactly as they do
  today, whether or not a `base` is declared.
- Never silently skip work: whenever Alba cannot prove a reduced list is enough, the beam runs on everything.

## Non-goals

- Guessing the parent branch of a branch. Git does not record it; the reflog is local and expires, and the
  "closest candidate" heuristic ties in git-flow exactly in the `hotfix/*` case. The reference is declared.
- List literals (`['a', 'b']`) and general list operations (`filter`, `map`, `join`). Only what this feature
  needs ships: the list type, `git.changes()`, and `.dirs()`. The method-call grammar leaves room for more.
- Finding modules by a marker file (`within: 'package.json'`). `.dirs('glob')` covers regular layouts at any
  depth; a marker-based reduction can be added later as another method.
- Renaming `--force` to `--no-cache`: done separately in
  [albahq/alba#50](https://github.com/albahq/alba/pull/50).

## Anti-goals

- A beam that passes on a reduced list must never let a full run be answered from the cache. The rendered
  commands are already part of the cache fingerprint (`crates/alba-engine/src/cache/fingerprint.rs`), and the
  reduced list is part of the rendered command, so an incremental entry and a full entry never share a
  fingerprint. This must stay true; a test guards it.
- Configuration changes (`phpcs.xml`, `phpstan.neon`) must not be passed to the tool as files to process.

## Key decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Where the reference comes from | Declared in the Beamfile, overridable by `--affected <ref>` | The rule depends on the branch and belongs with the beams; CI can still force one |
| Choosing a value by branch | A `match` expression with glob arms | One subject, flat arms, globs checked at load time; covers what a `matches` operator would, so none is added |
| Diff origin | Merge base with the reference, for every git diff Alba takes, `--affected` included | Counting commits that landed on the target after the branch was cut is the bug the issue reports |
| How a beam reads its files | `git.changes('glob')`, a method of the `git` object | It is git information; the `git` object is the git plugin's namespace |
| What a config change does | Full mode: the call returns the glob's base directory | A rule change concerns every file |
| Reducing to modules | A generic list method, `.dirs('glob')` | Single responsibility: listing changes and reducing paths are separate operations |
| Selection versus mode | Separate: selection unchanged, `git.changes()` incremental by default when a `base` is declared | Narrowing the selection by default would stop beams with no `inputs` (`deploy`, hooks) from ever running; the cache already skips untouched beams |
| Opting out | `--no-affected` | Pairs with `--affected` the way `--no-ui` pairs with `--ui`; `--force` already means "ignore the cache" |
| Where `base` is declared | A `git { ... }` block, root Beamfile only | Room for future git settings without new top-level keywords; one reference per project |
| Several patterns in one `match` arm | `'hotfix/*' \| 'master' => ...` | Strings are templates, so a `{a,b}` glob alternative would need `{{a,b}}`; `\|` reads better and needs no new lexing rule |

## Approaches considered

- **One change set.** Everything in one pull request. Nothing ships half done, but the review spans the parser,
  the evaluator, git, the engine and the CLI, and a regression is hard to isolate. Rejected.
- **Three increments, each useful alone.** Chosen; see [Increments](#increments).
- **One increment per building block** (lists alone, method calls alone, ...). Rejected: several blocks would
  ship with no use, which is speculative abstraction.

## Increments

Each increment is production-grade on its own and leaves the repository releasable.

1. **Merge base for `--affected`.** `changed_files` diffs from the merge base. A behaviour change, recorded in the
   changelog as such.
2. **`match` and parser-driven interpolation.** Useful alone (in a `let`, a `description`, an executor option).
3. **Incremental beams.** The list type, method calls, `.dirs()`, the `git { base }` block, `git.changes()`, the
   incremental default and `--no-affected`. These pieces only make sense together.

## Increment 1: merge base

`alba_core::git::changed_files(cwd, reference)` today runs `git diff --name-only -z <reference> --`. It becomes:

1. `git merge-base <reference> HEAD`, giving `<base>`. Run as its own command rather than
   `git diff --merge-base`, which needs git 2.30.
2. `git diff --name-only -z <base> --`, unchanged otherwise (working tree, index, deletions, renames).
3. Untracked files as today.

Errors: no merge base (unrelated histories, a shallow clone that does not reach it) is a `GitError` naming the
reference and suggesting `git fetch --unshallow` or a deeper fetch. The existing refusal of a reference starting
with `-` is kept and applies before `merge-base` too.

The README's [Affected runs](../../../README.md#affected-runs) section says the diff is taken from the merge base,
and the changelog entry says it changed.

## Increment 2: `match` and interpolation

### `match`

```
let base = match git.branch {
  'hotfix/*'  => 'origin/master'
  'release/*' => 'origin/main'
  _           => 'origin/develop'
}
```

- The subject is any expression evaluating to a string; a boolean subject is a type error.
- Each arm is `_` or one or more string literals separated by `|`, each holding a glob in the `globset` dialect
  `inputs` uses: `'hotfix/*' | 'master' => 'origin/master'`. Patterns are ordinary strings, so a literal brace is
  written `{{` as everywhere else; an interpolation inside a pattern is a parse error, since patterns must be
  known at load time. Every pattern is compiled and validated at load time, on every path, like `glob()`.
- Arms are tried in order; the first match wins. A glob `*` does not cross `/`, so `'hotfix/*'` matches
  `hotfix/x` but not `hotfix/x/y`; `'hotfix/**'` does.
- `_` is mandatory and must be the last arm. A missing `_`, an arm after `_`, or a duplicate pattern is a parse
  error pointing at it.
- Arm values are full expressions and may differ in type only if the context accepts both (same rule as `if`).
- Arms are separated by newlines or commas.
- `match` is reserved as a keyword.

`check_expr` validates every arm, like it validates both branches of an `if`: a typo in an arm not taken on this
machine still fails the load.

### Interpolation

`scan_interpolation` (`crates/alba-syntax/src/template.rs`) currently collects tokens up to the first `RBrace`,
then parses them. A `match` body contains braces, so that cut is wrong. The new order:

1. Lex the rest of the string literal.
2. Parse one expression; the parser consumes the `match` braces itself.
3. Require an `RBrace` right after it.

"unclosed interpolation" and "empty interpolation" keep their spans and messages. An expression followed by
anything other than `}` is a new error, "expected `}` to close the interpolation", pointing at the extra token.
No string valid today changes meaning: the expression grammar had no other use for braces. `{{` and `}}` stay
literal braces outside interpolations.

## Increment 3: incremental beams

### The `git` block

```
git {
  base env('GITHUB_BASE_REF', match git.branch {
    'hotfix/*' => 'origin/master'
    _          => 'origin/develop'
  })
}
```

- A top-level declaration, at most one per project, in the root Beamfile only. A second block, or a block in an
  imported Beamfile, is a load error pointing at it.
- `base` is an expression evaluated at load time, with the load-time rules (`env()` needs a default).
- `base` may read `git.branch`, `git.sha`, `git.short_sha`, `git.dirty` and any `let` binding of the file:
  every `let` is evaluated before the block. Calling `git.changes()` there is a load error.
- If git cannot answer while `base` is evaluated (no repository, as in a Docker build context without `.git`; no
  commit yet; git missing), the project loads with no reference and a warning naming git's message, instead of
  failing to load. Outside the block, reading `git.*` keeps today's behaviour: a load error.
- An empty string as `base` means "no reference": the project behaves as if no block were declared. This lets a
  `match` arm opt a branch out (`'main' => ''`).
- The evaluated `base` is not exposed as a `git.base` field: nothing needs it, and a field would disagree with the
  reference actually in effect whenever the command line overrides it.
- Unknown keys inside the block are load errors with a "did you mean" suggestion, as executor options are.

### Lists and method calls

- `Value` gains a `List(Vec<String>)` variant. No literal syntax produces one; only `git.changes()` and list
  methods do.
- The grammar gains method calls: `<expr>.<name>(<args>)`. Field access (`git.branch`) is unchanged. A method
  call on a value that has no such method is a load error naming the value's type and its methods.
- `git.changes(...)` is a method call on the `git` object.
- A list is accepted only in `run` and `env` templates. In a template it renders as its elements, each quoted
  for the shell that will run the command, joined by single spaces. Anywhere a string or a boolean is required
  (`if` condition, `match` subject, `==`, `+`, `inputs`, `cwd`, `description`), a list is a type error.
- Quoting depends on the executor: POSIX single quoting for the embedded shell, `sh`, and docker and compose
  commands; PowerShell single quoting (`'` doubled) for `system_shell` on Windows. A plugin executor receives
  POSIX quoting. An element needing no quotes is left bare, so logs stay readable.

### `git.changes(glob)`

Returns the changed files of this beam matching `glob`, as a list.

Validation at load time:

- Allowed only in `run` and `env`, like `env()` without a default. Elsewhere it is a load error explaining that
  its value belongs to the moment the beam runs.
- Exactly one string argument, a literal or a `let`, so it is known at load time and compiled like `glob()`.
- The glob must equal one of the beam's `inputs`, compared after rendering (both are known at load time, since
  `inputs` cannot read parameters). Otherwise it is a load error listing the beam's inputs. This keeps the cache,
  affected selection and the list consistent.

Value at run time, in this order:

1. **No reference** (no `git` block, empty `base`, or `--no-affected`): full mode.
2. **Reference unresolvable** (unknown reference, no merge base, git unavailable): full mode, with a warning
   naming the reference and git's message, printed once per run.
3. **Merge base is `HEAD` and the working tree is clean** (running on the target branch itself, typically in CI
   after a merge): full mode. With uncommitted work (a fresh branch, a local target branch being edited), the
   mode stays incremental over that work.
4. **The beam's Beamfile changed, or a beam it transitively `needs` is affected** (a `composer.lock` change
   reaching an `install` beam may change the tool itself): full mode.
5. **A changed path matches one of the beam's `inputs` but none of the globs the beam passes to any
   `git.changes()` call** (a `phpcs.xml` change): full mode.
6. Otherwise: incremental mode.

In incremental mode the list holds the changed paths matching `glob` that still exist on disk (an added,
modified, renamed or untracked file; a deleted file is dropped; a rename keeps its new path), sorted, without
duplicates. In full mode it holds one element: the glob's base directory, from `alba_core::pattern_dir`
(`src/**/*.php` gives `src`; a glob with no literal prefix gives `.`).

Paths are relative to the beam's rendered `cwd`, using `/` on every platform for the embedded shell and `sh`,
and the native separator for `system_shell` on Windows.

A beam in incremental mode whose every `git.changes()` call is empty (none of its files changed, or only deleted
ones) does not run. It ends with a new status, `unchanged`:

- It satisfies the `needs` of its dependents, like `succeeded` and `cached`.
- It writes no cache entry and reads none: nothing ran, so there is nothing to record.
- It does not affect the exit code.
- The JSON stream emits a `beam_finished` with status `unchanged`, and `run_finished` gains an `unchanged` list.
- The text renderers and the interface show it as `○ unchanged`.

A beam with several calls runs when at least one is non-empty; the empty ones render as nothing.

### `.dirs(glob)`

A list method: each element becomes the shortest prefix of its path, cut at a `/` boundary, that matches `glob`.

- `glob` is a literal or a `let`, compiled at load time.
- An element with no matching prefix is kept unchanged. This is what makes full mode work: `src` is shorter than
  `src/*` and stays `src`, so phpstan analyses everything.
- Then duplicates are removed, and an element under another kept element is dropped (`src` and `src/Billing`
  give `src`), so the tool never processes a directory twice. The result is sorted.

```
run "vendor/bin/phpstan analyse {git.changes('src/**/*.php').dirs('src/*')}"
```

### Run selection and mode

Selection (which beams run) and mode (what `git.changes()` returns) are decided separately.

**Selection is unchanged.** `alba run [beam]`, a bare `alba` (the `default` beam) and `alba hook <name>` run the
requested subgraph exactly as today, whether or not a `base` is declared; the cache keeps skipping beams whose
fingerprint did not move. Only `--affected` narrows the selection, and its argument becomes optional:

| Command | Selection | Mode of `git.changes()` |
| --- | --- | --- |
| `alba run [beam]`, `base` declared and non-empty | the subgraph | incremental against `base` |
| `alba run [beam]`, no `base` | the subgraph | full |
| `alba run --affected [beam]` | affected against `base`; startup error (exit `2`) when none is declared | incremental against `base` |
| `alba run --affected <ref> [beam]` | affected against `<ref>` | incremental against `<ref>` |
| `alba run --no-affected [beam]` | the subgraph | full |
| `alba affected` | lists the beams affected against `base`; error when none is declared | n/a |
| `alba affected <ref>` | lists the beams affected against `<ref>` | n/a |

- `--affected` and `--no-affected` conflict (clap `conflicts_with`).
- `--affected` without a value followed by a beam name is ambiguous for clap (`--affected phpcs`). The builder picks
  the parsing that keeps `alba run --affected <ref> <beam>` working as today and documents how to pass a beam
  with the declared reference (for example `--affected=` or a beam before the flag); a test covers both forms.
- A declared `base` that cannot be resolved (unknown reference, no merge base) gives full mode with a warning when
  it only drives the mode, and a startup error (exit `2`) when `--affected` asked it to drive the selection, as an
  unresolvable `--affected <ref>` is today.
- `run_started` in the JSON stream keeps `affected_by` (set only when `--affected` narrowed the selection) and gains
  `base`: the reference in effect and its merge-base commit, omitted when there is none. Each `beam_started` gains
  `mode`, `incremental` or `full`, for beams that call `git.changes()`, with `full_reason` when full.
- The interface shows the reference in effect in the run header.

### With `--watch`

Every run of a watch session, initial or triggered, recomputes the merge base, the changed files and each beam's
mode, as `--affected --watch` already recomputes the targets. The watched set stays the whole subgraph.

## Where it lives

| Crate | Change |
| --- | --- |
| `alba-syntax` | `match` expression, method-call expression, `git { }` declaration, interpolation parsing |
| `alba-core` | `Value::List`, `match` and method evaluation and validation, the `git` block, `git.changes()` validation, `.dirs()`, merge-base in `git.rs` |
| `alba-engine` | Per-run git change set, per-beam mode, rendering `git.changes()` at schedule time, the `unchanged` status, selection with an optional `--affected` reference |
| `alba` | `--no-affected`, optional reference on `--affected` and `alba affected`, JSON fields, text rendering of `unchanged` |
| `alba-tui` | Reference in the header, the `unchanged` status |
| `alba-agent-plugin` | The `using-alba` skill documents `git { base }`, `match`, `git.changes()`, `.dirs()` and `--no-affected` |

The change set is computed once per run (one `merge-base`, one `diff`, one `ls-files`) and shared by every beam,
like `LazyGit` shares `git.head()` across a load.

## Error handling

- Every load error carries a span and, where one exists, a `help:` suggestion, as the evaluator's errors do today.
- A git failure on a declared `base` is a warning and a full run, never a silent incremental run on a guessed
  list. A git failure on an explicit `--affected <ref>` is an error.
- A beam's mode and the reason for full mode (`no reference`, `on the target branch`, `phpcs.xml changed`,
  `Beamfile changed`, `affected through needs`) are logged at debug level and carried in the JSON stream, so a
  surprising full run can be diagnosed.

## Testing strategy

Test-first, through the public entry points: the CLI (`crates/alba/tests/`), the engine's run session, and the
loader for load-time diagnostics. Git is real in tests (temporary repositories), as the existing affected tests do.

- Merge base: a branch cut from `develop`, then a commit on `develop`; the later commit is not counted.
- `match`: first match wins, `_` required, globs validated in arms not taken, braces inside an interpolation.
- `git.changes()`: incremental list, deleted files dropped, rename keeps the new path, untracked counted, a config
  input forces full mode, Beamfile change forces full mode, affected through `needs` forces full mode, merge base
  at `HEAD` on a clean tree forces full mode, unresolvable base warns and runs full, every call empty ends the
  beam `unchanged`.
- `.dirs()`: reduction at one and two levels, full-mode passthrough, nested elements collapsed.
- Quoting: a file name with a space reaches the command as one argument on the embedded shell and on
  `system_shell`.
- Cache: an incremental pass followed by `--no-affected` on the same tree reruns the beam.
- Unchanged: a beam whose calls are all empty ends `unchanged`, its dependents run, the exit code is `0`, no cache
  entry is written.
- Selection: with a `base` declared, `alba run deploy` (no `inputs`) and `alba hook commit-msg` still run.
- Git block: a Beamfile with a `git` block loads outside a repository, with a warning and no reference.
- Working tree: a fresh branch with uncommitted changes stays incremental; a clean checkout of the target is full.
- CLI: `--affected` and `--no-affected` conflict; `--affected` without a reference uses `base`, both with and
  without a beam name; `alba affected` without a reference uses `base`.

## Success criteria

The issue's workaround becomes:

```
git {
  base match git.branch {
    'hotfix/*' | 'master' => 'origin/master'
    _                     => 'origin/develop'
  }
}

beam phpcs {
  inputs ["src/**/*.php", "phpcs.xml"]
  run "vendor/bin/phpcs {git.changes('src/**/*.php')}"
}

beam phpstan {
  inputs ["src/**/*.php", "phpstan.neon"]
  run "vendor/bin/phpstan analyse {git.changes('src/**/*.php').dirs('src/*')}"
}
```

with no `system_shell`, no intermediate file, and `alba run phpcs` linting only the PHP files the branch changed.

## Calls made by the designer

Settled while writing, not discussed with the user one by one; listed so they can be checked:

- Several patterns per `match` arm with `|`, instead of `{a,b}` glob alternatives.
- The `git.changes()` glob is compared to `inputs` after rendering.
- Git unavailable while `base` is evaluated: load with no reference and a warning.
- Full mode on the target branch only when the working tree is clean.
- No `git.base` field.
- A beam affected through `needs` runs in full mode.
- The `unchanged` status and its semantics.
- An empty `base` means no reference.
- List quoting per executor, and `/` separators except for `system_shell` on Windows.
- A git failure on a declared `base` warns and runs full; on an explicit `--affected`, it is an error.

## Left to the builder

- Internal representation of the per-run change set and how it reaches the renderer.
- Exact wording of new diagnostics, following the existing style.
- How the interface lays out the reference and mode in the header.
