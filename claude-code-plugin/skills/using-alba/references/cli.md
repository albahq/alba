# alba CLI

```sh
alba                          # runs `default`, or lists beams when there is none
alba check                    # loads and validates the Beamfile, runs nothing
alba run <beam> [PARAM]...    # runs a beam and everything it needs
alba affected <ref>           # lists the beams a git diff affects, runs nothing
alba cache clean              # removes <beamfile dir>/.alba/cache
alba hooks install|uninstall  # git hooks declared by the Beamfile
alba plugin check <BINARY>    # drives an executor plugin through the protocol
```

Global: `--file <PATH>` (default `./Beamfile`), `-h`, `-V`.

On a terminal, every command opens the interactive interface. Off a
terminal (how Claude runs it) output is plain text. `--no-ui` forces the
text renderers on a terminal.

## `alba run` flags

| Flag | Meaning |
| --- | --- |
| `--jobs <N>` | Max beams at once (default: available parallelism). |
| `--keep-going` | Do not cancel pending beams after a failure. |
| `--force` | Ignore the cache for this run; a success is still written back. |
| `--output interleaved\|grouped` | Text layout. Default `grouped` off a terminal. |
| `--log-format text\|json` | `json`: one event object per line. |
| `--watch` | Re-run on changes to the subgraph's `inputs` or any loaded Beamfile. Debounced 200 ms. |
| `--affected <REF>` | Run only beams affected since `<REF>`, plus their dependents. |
| `--ui` / `--no-ui` | Force the interactive interface on or off. |

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | A beam failed and was not `allow_failure`. |
| `2` | Alba itself failed: missing Beamfile, parse or validation error, bad invocation. |
| `130` | Interrupted. |

`alba check` and `alba affected` exit `0` when nothing is wrong, even with
an empty affected list. A `--watch` session exits `0` on Ctrl-C.

## JSON stream (`--log-format json`)

One object per line, dispatched on `event`:

| `event` | Fields |
| --- | --- |
| `run_started` | `targets`, `affected_by`, `beams`, `edges` (`[beam, dependency]` pairs). Always first. |
| `beam_started` | `beam`. |
| `beam_cached` | `beam`. |
| `beam_output` | `beam`, `stream` (`stdout`/`stderr`), `text`, `replayed`. |
| `beam_finished` | `beam`, `status`, `exit_code`, `duration_ms`. |
| `run_finished` | `succeeded`, `cached`, `failed`, `failed_allowed`, `cancelled` (beam id lists), `duration_ms`, `exit_code`. |

A watch session adds `project_broken` (`diagnostic`), `watch_waiting`
(`files`), and `watch_triggered`.

`alba affected <ref> --log-format json` prints `{"beams": [...]}`.

## Affected runs

A beam is affected when one of its `inputs` globs matches a path changed
since `<ref>` (tracked diff plus untracked, non-ignored files), when its
declaring Beamfile changed, or transitively when it `needs` an affected
beam. A beam with no `inputs` is never affected on its own. Requires `git`
on the `PATH`.

## Caching

A beam with `inputs` is skipped when its matched files, rendered command,
`cwd`, `env`, arguments, executor options, and every dependency's
fingerprint are unchanged since the last success; its stored output is
replayed and it counts as `cached`. Cache lives under
`<beamfile dir>/.alba/cache`.
