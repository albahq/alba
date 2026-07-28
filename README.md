# Alba

Alba is a task runner. Build files are described by a **Beamfile**, and a
**beam** is the unit of work it executes: a named task with a shell command,
optional dependencies on other beams, and a few small conveniences (string
templates, parameters, environment variables) for keeping a build script
readable.

## Install

```sh
cargo install --path crates/alba-cli
```

This builds the `alba` binary and installs it into Cargo's bin directory
(`~/.cargo/bin` by default). Alba has no other runtime dependencies.

## A Beamfile

Save this as `Beamfile` in a project's root:

```
let profile = env("PROFILE", "debug")

default check

beam build {
  description "Compile the project"
  run "echo building in {profile} mode"
}
beam test {
  needs [build]
  run "echo tests passed"
}
beam lint {
  allow_failure true
  run "false"
}
beam greet(name) {
  description "Greet someone by name"
  run "echo hello, {name}!"
}
beam check {
  description "Everything that must pass before shipping"
  needs [test, lint]
  run "echo all checks passed"
}
```

A beam's `run` command is a string template: `{profile}` interpolates the
`let` binding above, and `{name}` interpolates `greet`'s own parameter.
`needs` lists the beams that must succeed first; `allow_failure` lets a beam
fail (`lint` here) without failing the beams that depend on it. `default`
names the beam a bare `alba` runs when no subcommand is given.

### Interpolation

Anything between `{` and `}` in a string is an expression. Inside those
braces, write string literals with **single** quotes: the body is read
straight from the source, so `"{env(\"VAR\")}"` is a syntax error while
`"{env('VAR')}"` is what you want. Escapes work normally everywhere else in
the string, and `{{` / `}}` produce literal braces.

### Reading the environment

`env("NAME", "fallback")` reads an environment variable, using the fallback
when it is unset. Written without a fallback, `env("NAME")` is resolved only
when a beam actually runs, so it may appear in `run` commands and in `env`
values but not in a `let` binding, `description`, `inputs`, `outputs`, `cwd`,
or an executor option — those are resolved while the Beamfile loads, and one
unset variable there would make the whole project fail to load, including for
beams nobody asked to run. Alba rejects that at the call site rather than
letting `alba check` answer differently depending on the machine it runs on.

## Usage

```sh
alba              # runs the declared `default` beam, or lists beams if none is declared
alba check        # loads and validates the Beamfile without running anything
alba run <beam>   # runs a beam and everything it needs
```

Running the example above:

```sh
$ alba check
✓ Beamfile: 5 beams

$ alba run greet World
── greet (0.0s, ok) ──
hello, World!
✓ 1 succeeded · 0.0s

$ alba
── lint (0.0s, failed (allowed)) ──
── build (0.0s, ok) ──
building in debug mode
── test (0.0s, ok) ──
tests passed
── check (0.0s, ok) ──
all checks passed
✓ 3 succeeded · ⚠ 1 failed (allowed) · 0.0s
```

`build` and `lint` have no `needs` of their own, so they run concurrently and
race to finish first — running this yourself, don't be surprised if their
lines come out in the opposite order. The rest (`test` waiting on `build`,
`check` waiting on both) is always ordered the same way.

### Flags

Common to every subcommand:

| Flag | Description |
| --- | --- |
| `--file <PATH>` | Path to the Beamfile to load. Defaults to `./Beamfile` in the current directory when omitted. |
| `-h`, `--help` | Print help. |
| `-V`, `--version` | Print version. |

`alba run <beam> [PARAM]...` additionally accepts:

| Flag | Description |
| --- | --- |
| `[PARAM]...` | Positional arguments bound, in order, to the parameters the beam declares (`beam deploy(target)` takes one). |
| `--jobs <N>` | How many beams may run at once. At least 1; defaults to the machine's available parallelism. |
| `--keep-going` | Keep going after a beam fails, instead of cancelling the beams that have not started. |
| `--output <STYLE>` | How text output is laid out: `interleaved` (every line as it happens, prefixed with the beam it came from) or `grouped` (each beam's output held back and printed as one block when it ends). Defaults to `interleaved` on a terminal and `grouped` otherwise. Ignored with `--log-format json`. |
| `--log-format <FORMAT>` | What stdout carries: `text` (human-readable, laid out by `--output`) or `json` (one JSON object per event, one per line). Defaults to `text`. |

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | A beam failed (and was not `allow_failure`). |
| `2` | Alba itself failed: a missing Beamfile, a parse or validation error, a bad invocation, an unschedulable run. |
| `130` | The user interrupted the run (Ctrl-C). |

### Executors

A beam runs its command in a shell by default. `executor docker { image
"..." }` also parses, but running it currently fails at run time with
`docker executor is not yet supported` — the docker executor is not
implemented yet.

## Caching

A beam that declares `inputs` is cached. Before running it, Alba checks
whether the files matched by `inputs`, the rendered command, the beam's
`cwd`, its `env`, its arguments, and every dependency's own fingerprint are
all unchanged since the last successful run. If so, the beam is skipped:
it is reported as `cached` in the output, its stored logs are replayed in
its place, and the run's summary counts it under `cached` rather than
`succeeded`. Change any of those inputs, even a single character in one
matched file, and the beam runs again.

A beam without declared `inputs` is never cacheable and always runs. This
is deliberate for beams whose own work is cheaper than hashing their
inputs would be, such as an umbrella beam that only reports on the beams
it needs. If a beam declares `outputs` and one of them is missing from
disk, the beam also reruns even if its inputs are otherwise unchanged: a
cache entry only stands in for work whose result is actually still there.

`alba run <beam> --force` ignores the cache on the way in: the beam runs
regardless of what changed, and its result is written back to the cache
for the next invocation to read. Use it to force a rebuild without
clearing history for every other beam.

The cache itself lives on disk under `<beamfile directory>/.alba/cache`.
`alba cache clean` removes it entirely; the next run of any beam starts
from scratch and repopulates it. Alba never edits your `.gitignore`, so
add `.alba/` to it yourself in any project that turns caching on.
