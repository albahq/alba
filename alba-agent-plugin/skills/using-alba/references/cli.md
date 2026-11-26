# alba CLI

```sh
alba                          # runs `default`, or lists beams when there is none
alba check                    # loads and validates the Beamfile, runs nothing
alba run <beam> [PARAM]...    # runs a beam and everything it needs
alba affected [ref]           # lists the beams a git diff affects, runs nothing
alba cache clean              # removes <beamfile dir>/.alba/cache
alba hooks install|uninstall  # git hooks declared by the Beamfile
alba plugin check <BINARY>    # drives an executor plugin through the protocol
```

Global: `--file <PATH>` (default `./Beamfile`), `-h`, `-V`.

On a terminal, every command opens the interactive interface. Off a
terminal (how coding agents commonly run it) output is plain text. `--no-ui` forces the
text renderers on a terminal.

## `alba run` flags

| Flag | Meaning |
| --- | --- |
| `--jobs <N>` | Max beams at once (default: available parallelism). |
| `--keep-going` | Do not cancel pending beams after a failure. |
| `--no-cache` | Ignore the cache for this run; a success is still written back. `--force` is a deprecated spelling that warns on stderr. |
| `--output interleaved\|grouped` | Text layout. Default `grouped` off a terminal. |
| `--log-format text\|json` | `json`: one event object per line. |
| `--watch` | Re-run on changes to the subgraph's `inputs` or any loaded Beamfile. Debounced 200 ms. |
| `--affected [<REF>]` | Run only beams affected since `<REF>`, plus their dependents. Without a value, the declared `git { base }`; an error when none is declared. `--affected lint` reads `lint` as the reference: write `alba run lint --affected` or `--affected= lint`. |
| `--no-affected` | Every `git.changes()` call runs in full mode, whatever `base` is declared. Conflicts with `--affected`. |
| `--ui` / `--no-ui` | Force the interactive interface on or off. |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | A beam failed and was not `allow_failure`, or an executor could not release what it held for the run. |
| `2` | Alba itself failed: missing Beamfile, parse or validation error, bad invocation. |
| `130` | Interrupted: Ctrl-C, `SIGTERM`, or `SIGHUP`. |

`alba check` and `alba affected` exit `0` when nothing is wrong, even with
an empty affected list. A `--watch` session ended by Ctrl-C exits `0`, or
`1` when an executor could not release what it held once the session
ended; `2` when the Beamfile fails to load at startup or the file watcher
cannot start or dies, and `2` wins over `1`. A second Ctrl-C leaves at
once and skips that cleanup.

## JSON stream (`--log-format json`)

One object per line, dispatched on `event`:

| `event` | Fields |
| --- | --- |
| `run_started` | `targets`, `affected_by`, `base` (`{"reference", "merge_base"}`, omitted without a resolved reference), `beams`, `edges` (`[beam, dependency]` pairs). Always first. |
| `beam_started` | `beam`; for a beam calling `git.changes()`, `mode` (`incremental`/`full`) and, when full, `full_reason`: `no_reference`, `reference_unresolved`, `on_target_branch`, `beamfile_changed`, `dependency_affected`, `input_outside_changes`. |
| `beam_cached` | `beam`. |
| `beam_output` | `beam`, `stream` (`stdout`/`stderr`), `text`, `replayed`. |
| `beam_finished` | `beam`, `status` (`succeeded`, `cached`, `unchanged`, `failed`, `failed_allowed`, `cancelled`), `exit_code`, `duration_ms`. An `unchanged` beam had no `beam_started`. |
| `run_finished` | `succeeded`, `cached`, `unchanged` (omitted when empty), `failed`, `failed_allowed`, `cancelled` (beam id lists), `duration_ms`, `exit_code`, `cleanup_errors` (a non-empty list makes the exit code `1`). |
| `warning` | `message`. Right after `run_started`, once per run, only when the run warns. |

A watch session adds `project_broken` (`diagnostic`), `watch_waiting`
(`files`), and `watch_triggered`. Its `cleanup_errors` are always empty:
executors are shut down when the session ends, and their errors go to
stderr as `cleanup failed: <message>` lines.

`alba affected [ref] --log-format json` prints `{"beams": [...]}`.

## Affected runs

A beam is affected when one of its `inputs` globs matches a path changed
since `<ref>` (tracked diff plus untracked, non-ignored files, anywhere in
the repository, so `../` inputs and imports count), when its
declaring Beamfile changed, or transitively when it `needs` an affected
beam. A beam with no `inputs` is never affected on its own. Requires `git`
on the `PATH`. A `<ref>` starting with `-` is refused. The diff starts at
the merge base of `<ref>` and `HEAD`. Without `<ref>`, `--affected` and
`alba affected` use the declared `git { base }`, and exit `2` when none is
declared or git cannot resolve it.

## Incremental runs (`git.changes()`)

Selection and mode are separate: only `--affected` narrows which beams
run. Every other run (bare `alba`, `alba run`, `alba hook`) runs its usual
subgraph, and `git.changes()` lists the files changed since the declared
`base`.

| Command | Selection | `git.changes()` |
| --- | --- | --- |
| `alba run [beam]`, `base` declared | the subgraph | incremental against `base` |
| `alba run [beam]`, no `base` | the subgraph | full |
| `alba run --affected [beam]` | affected against `base` (exit `2` without one) | incremental against `base` |
| `alba run --affected <ref> [beam]` | affected against `<ref>` | incremental against `<ref>` |
| `alba run --no-affected [beam]` | the subgraph | full |

A declared `base` git cannot resolve (shallow CI clone, unknown branch)
warns ``cannot diff from `<ref>`: ...; running in full mode`` and runs
full, exit code unaffected. A `git` block git could not evaluate warns
`<message>; no reference declared for this run`. Each run warns once: a
`warning: ...` stderr line in text, a `warning` event in JSON, a row under
the header in the interface.
A beam whose `git.changes()` calls are all empty ends `○ unchanged`: it
runs nothing, satisfies its dependents, touches no cache entry, and does
not affect the exit code. Every run of a `--watch` session recomputes the
changes and follows an edited `base`; `--affected <ref>` survives a retarget
from the interface. The interactive interface shows `· base <ref>` in its
header.

## Caching

A beam with `inputs` is skipped when its matched files, rendered command,
`cwd`, `env`, `env_inputs` values, arguments, executor options and
resolved executor state (see `beamfile.md`, Executors), and every
dependency's fingerprint are unchanged since the last success, and its
declared `outputs` are still what that run left. Its stored output is
replayed and it counts as `cached`. Since alba 0.6.0, upgrading Alba
invalidates every entry, and on unix the cache is readable by its owner
only. The cache lives under `<beamfile dir>/.alba/cache`.
