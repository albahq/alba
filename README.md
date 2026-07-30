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
| `--watch` | Keep running: re-run the beam whenever the files its subgraph declares as `inputs` change, or a loaded Beamfile changes. Ctrl-C ends the session. See [Watch mode](#watch-mode). |

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | A beam failed (and was not `allow_failure`). |
| `2` | Alba itself failed: a missing Beamfile, a parse or validation error, a bad invocation, an unschedulable run. |
| `130` | The user interrupted the run (Ctrl-C). |

`alba run --watch` reports differently, because a session outlives any single
run inside it: every run already reported its own outcome as it happened, so
there is nothing left to score at the process level once the session ends.
An orderly Ctrl-C ends the session with `0`. `2` covers whatever keeps the
session from being a session at all: a Beamfile that fails to load at
startup, a file watcher that cannot start, or one that dies partway through
and leaves nothing left to watch.

### Executors

A beam runs its command on Alba's embedded shell by default (see
[Embedded shell](#embedded-shell) below); `executor system_shell` opts out
to the host shell instead. `executor docker { image "..." }` also parses,
but running it currently fails at run time with `docker executor is not
yet supported`, since the docker executor is not implemented yet.

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
Both `inputs` and `outputs` patterns resolve relative to the directory of
the Beamfile that declares the beam, never to the beam's `cwd`, so a beam
with `cwd "sub"` writing `out.txt` must declare it as `outputs
["sub/out.txt"]`.

`alba run <beam> --force` ignores the cache on the way in: the beam runs
regardless of what changed. If it succeeds, its result is written back to
the cache for the next invocation to read; a forced run that fails leaves
the previous entry untouched. Use it to force a rebuild without clearing
history for every other beam.

The cache itself lives on disk under `<beamfile directory>/.alba/cache`.
`alba cache clean` removes it entirely; the next run of any beam starts
from scratch and repopulates it. Alba never edits your `.gitignore`, so
add `.alba/` to it yourself in any project that turns caching on.

## Watch mode

`alba run --watch [beam]` runs the beam, then keeps the session open and
re-runs it whenever a relevant file changes. Like a plain `alba run`, an
omitted beam falls back to the declared `default`, so a bare
`alba run --watch` works too.

What counts as relevant is exactly what the cache already tracks: the
files the target's subgraph declares as `inputs`, the same declarations
the cache fingerprints, so watch mode and the cache can never disagree
about what a change is. Every Beamfile the project loaded is
watched as well, so editing the Beamfile itself also triggers a run. A file
that `.gitignore` excludes never triggers one, for the same reason it never
enters the cache's fingerprint, and `.alba/` and `.git/` are excluded
outright: the cache writing its own state must never wake the session that
owns it. If no beam in the target's subgraph declares any `inputs`, Alba
prints a warning once at startup, since such a session can still only react
to a Beamfile edit.

A change that lands while a run is still in progress cancels that run and
starts a new one right away rather than waiting for it to finish. Every
change that lands during a run is folded into the next one, so a save the
cancellation raced with is never dropped.

Editing a loaded Beamfile reloads the project before the next run, so the
session always schedules the beams as they are currently written. If the
edit leaves the Beamfile unparsable, Alba renders the same diagnostic
`alba check` would report for it, runs nothing, and sits idle until a later
save produces a Beamfile that parses again, at which point the session
resumes on its own. While it sits there, the diagnostic is printed once
rather than once per file change: it is reprinted only when a save actually
changes the answer.

`--force` only applies to the run the session starts with. Every run the
watcher triggers afterward reads the cache normally; forcing those too
would mean rerunning the whole subgraph on every keystroke, which defeats
the point of having a cache at all.

Changes are debounced for a built-in 200 ms before they trigger a run
(not a flag), so a save that touches several files at once, or an editor
that writes a file more than once, produces a single run rather than
several.

Ctrl-C ends the session in an orderly way: every run inside it already
reported its own outcome, so the process exits `0` rather than the `130`
a single interrupted `alba run` reports. See [Exit codes](#exit-codes).

On a terminal, the text renderers clear the screen before printing a
triggered run, so each run starts on a clean page. This never happens with
`--log-format json`, and never happens when stdout is not a terminal (a log
file, a pipe): there is no screen to clear for a reader parsing the stream,
and off a terminal the output accumulates as a record that clearing would
destroy.

Piped through `--log-format json`, a session's stream carries two event
kinds beyond an ordinary run's: `watch_waiting` (`files`, how many resolved
input files the session is watching) while it sits idle between runs, and
`watch_triggered` (`paths`, the changed paths that caused the next run) the
moment one starts. `paths` are relative to the project root, and the array
is empty when the trigger cannot be pinned to specific files, such as a
Beamfile that was broken and has just started parsing again. A session
parked on a project that will not load reports `watch_waiting` with `files`
at `0`: it is stopped, not running, and nothing is resolved while nothing
loads.

Two things are worth knowing before leaning on watch mode. A beam that
writes to a git-tracked file matched by its own `inputs` triggers itself on
every run and loops forever, so keep generated output git-ignored. And
watch roots are computed once at startup, from the project root and the
directory of any Beamfile loaded from outside it: an import added
mid-session whose directory lies outside those roots is not watched until
the session is restarted.

## Embedded shell

A beam's `run` command executes on Alba's own embedded shell by default: a
home-grown, cross-platform, POSIX-like interpreter, not `sh` or
PowerShell. The point is that a single Beamfile behaves identically on
macOS, Linux, and Windows, instead of quietly depending on whichever shell
happens to be installed on the machine that runs it.

The supported syntax covers what most beam commands actually need:
sequencing (`cmd1 ; cmd2`, `cmd1 && cmd2`, `cmd1 || cmd2`, negation
`! cmd`), pipelines (`cmd1 | cmd2`), redirections (`>`, `>>`, `<`, `2>`,
`2>>`, `2>&1`), POSIX quoting (single quotes are literal, double quotes
allow expansions, backslash escapes a single character), variables
(`$VAR`, `${VAR}`, and `$?` for the exit code of the last completed
command; assignment with `FOO=bar`, which persists for the rest of that
`run` string, an environment prefix like `FOO=bar cmd`, which applies to
that one command only, and the `export`/`unset` builtins), command
substitution
(`$(...)`, which shares the shell's state rather than running in an
isolated subshell, so `$(cd sub)` really does leave the shell in `sub`),
tilde expansion (`~` at the start of a word), and globbing (`*`, `?`,
`[...]`).

Sixteen builtins ship with the shell: the pure shell builtins `cd`, `pwd`,
`exit`, `true`, `false`, `export`, and `unset`; `echo`; the file builtins
`cat`, `cp`, `mv`, `rm`, `mkdir`, and `touch`; and the utilities `sleep`
and `test` (also spelled `[`). A builtin always wins over a PATH binary of
the same name, so `rm -rf dist` behaves the same on every platform even
where a system `rm` also exists; an explicit path such as `/bin/rm`
bypasses the builtin and reaches the system binary directly.

A few behaviors are deliberately frozen rather than left
implementation-defined: an unset variable expands to the empty string
(there is no `set -u`); a pipeline's exit code is always its last
command's (there is no `pipefail`); a glob that matches nothing is left
literal instead of disappearing or erroring; a wildcard never matches a
leading dot, so `*` skips hidden entries and a hidden entry is reached
only by writing its dot out (`.env`, `.h*`); `echo` recognizes only the
`-n` flag and never interprets backslash escape sequences; and a newline
inside a `run` string behaves exactly like `;`.

Shell control flow (`if`, `for`, `while`, `case`), functions, heredocs,
background jobs (`&`, `wait`), subshells (`(...)`), advanced expansions
such as `${VAR:-default}`, arithmetic `$((...))`, or brace expansion
`{a,b}`, and every special parameter but `$?` (`$$`, `$!`, `$#`, `$*`,
`$@`, `$1` to `$9`) are out of subset. None of them fail silently or fall
back to a system shell: each is a parse error with a span pointing at the
offending construct and a suggestion. For example:

```
$ alba check
beam `deploy`: invalid embedded shell command
error: `for` loops are not supported by the embedded shell
  │ for f in *.rs; do echo $f; done
  │ ^^^
  = help: move the logic into a script invoked by `run`, or declare `executor system_shell` on this beam
```

A beam whose command needs more than this subset can opt out with
`executor system_shell`, which runs its command through the host shell
(`sh` on Unix, PowerShell on Windows) instead, exactly as Alba did before
the embedded shell existed.

`alba check` parses the command of every beam that uses the embedded
shell and takes no parameters, catching unsupported syntax before a beam
ever runs rather than partway through a build. Parameterized beams (their
`run` template cannot be rendered until their arguments arrive),
`executor system_shell` beams, and `executor docker` beams are not
statically checked this way.

### Upgrading a Beamfile written for the host shell

The embedded shell is the default for every beam that does not say
otherwise, so a Beamfile written when `run` meant `sh -c` may need a
look. The breaks to expect, in rough order of how often they bite:

- **Control flow and functions.** `if`, `for`, `while`, `case`, and
  function definitions are out of subset. Move the logic into a script
  the beam invokes, or declare `executor system_shell` on that beam.
- **Special parameters.** Everything but `$?` is rejected: `$$`, `$!`,
  `$#`, `$*`, `$@`, and `$1` to `$9`. A command that built a temporary
  name out of `$$` needs another way to spell it.
- **Builtin flags.** The sixteen builtins have deliberately narrow flag
  surfaces (`rm -r -f`, `cp -r`, `mkdir -p`, `echo -n`, and nothing
  else), and a builtin always wins over the PATH binary of the same
  name. A flag outside that surface is a usage error rather than a
  silently different behavior. Reach the system binary with an explicit
  path (`/bin/rm`) when you really need one of its own options.
- **Hidden files.** `*` never matches an entry whose name starts with a
  dot, exactly as `sh` behaves. A beam that ran under PowerShell on
  Windows and counted on different wildcard rules has to name those
  entries.
- **Other out-of-subset syntax.** Heredocs, background jobs (`&`,
  `wait`), subshells (`(...)`), `${VAR:-default}`, arithmetic
  `$((...))`, and brace expansion `{a,b}`.

`alba check` catches the grammar half of this list statically, before
anything runs: unsupported syntax and rejected parameters are parse
errors. It cannot catch the runtime half, because an unsupported builtin
flag is a perfectly valid parse. `rm --one-file-system dist` parses
cleanly and fails only when the beam runs, so run the beam once to find
those.

Whenever the rewrite is not worth it, `executor system_shell` on that one
beam restores exactly the previous behavior, and the rest of the Beamfile
keeps the cross-platform guarantee.
