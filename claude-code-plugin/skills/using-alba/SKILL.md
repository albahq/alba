---
name: using-alba
description: This skill should be used when a Beamfile is present in the project, or when the user mentions Alba, beams, or the task runner, or asks to "build", "run", "test", "check", "create a beam", or "edit the Beamfile" in a project that uses Alba. Covers the Beamfile DSL, the alba CLI, and Alba's execution model.
---

# Using Alba

Alba is a task runner written in Rust, an alternative to `make`, `just`, and
`taskfile`. Tasks are called **beams** and are declared in a `Beamfile`.

## When to use

- A `Beamfile` exists at the project root.
- The user asks to build, run, test, check, or list tasks in an Alba project.
- The user asks to create or edit beams, or to migrate another task runner.

## Mental model

- **Beams** are named tasks in a `Beamfile`. Each runs one command (or a
  list of commands) as a string template: `{name}` interpolates a `let`
  binding or a beam parameter.
- **`needs` builds a DAG.** Independent beams run in parallel; a beam waits
  for everything it needs. `allow_failure true` lets a beam fail without
  failing its dependents.
- **`default <beam>`** is what a bare `alba` runs. Without one, bare `alba`
  lists the beams.
- **Caching is opt-in per beam** through `inputs` globs: a beam with
  `inputs` is skipped when nothing it depends on changed, and reruns when a
  declared `outputs` file is missing. A beam without `inputs` always runs.
- **Commands run on Alba's embedded shell** by default: a POSIX-like subset
  that behaves identically on macOS, Linux, and Windows. Shell control flow
  (`if`, `for`, `while`, `case`), functions, heredocs, and `$1`-style
  parameters are rejected at `alba check` time. A beam that needs them
  declares `executor system_shell`; `executor docker { image "..." }` runs
  inside a container; any other executor name is an external plugin binary.
- **`alba check`** validates the whole project without running anything.
  Run it after every Beamfile edit.

## Workflow

1. Read the `Beamfile` before changing it. Keep its existing style.
2. To run something: `alba run <beam> [params]`, or `alba check` to
   validate. Off a terminal (which is how Claude runs it), output is plain
   text; add `--log-format json` for one event per line.
3. After editing a Beamfile, run `alba check --file <path>` and fix every
   diagnostic before moving on. Exit code `2` means the Beamfile itself is
   invalid; `1` means a beam failed.
4. Prefer the embedded shell. Reach for `executor system_shell` only when
   a command really needs host shell features.

## References

- `references/beamfile.md`: the full Beamfile DSL, field by field.
- `references/cli.md`: every subcommand, flag, exit code, and the JSON
  stream.

The repository README (`https://github.com/albahq/alba`) is the source of
truth when a reference and the installed `alba --help` disagree.
