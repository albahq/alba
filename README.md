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

On a terminal, running any of these opens the interactive interface
described in [Interactive interface](#interactive-interface) instead of
printing lines like the ones below; what follows is what stdout carries
when it is not a terminal (redirected, piped, or under `--no-ui`).

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
| `--log-format <FORMAT>` | What stdout carries: `text` (human-readable, laid out by `--output`) or `json` (one JSON object per event, one per line). Defaults to `text`. See [The JSON stream](#the-json-stream). |
| `--watch` | Keep running: re-run the beam whenever the files its subgraph declares as `inputs` change, or a loaded Beamfile changes. Ctrl-C ends the session. See [Watch mode](#watch-mode). |
| `--ui` | Ask for the interactive interface even though stdout is not a terminal. Since it cannot actually draw there, the run is refused with an error instead of falling back to headless. Has no effect otherwise: on a terminal the interface is already the default, and `--log-format json`, `--output`, or `--no-ui` still choose the text renderers over it. See [Interactive interface](#interactive-interface). |
| `--no-ui` | Force the plain text renderers on, even on a terminal. |

### The JSON stream

`--log-format json` puts one JSON object per line on stdout. Every line
carries an `event` field naming its kind, so a consumer dispatches on that
before reading anything else. An ordinary run emits six kinds:

| `event` | Fields |
| --- | --- |
| `run_started` | `target` (the beam that was asked for), `beams` (every beam in its subgraph, the target included), `edges` (one `[beam, dependency]` pair per edge between them). Always the first line of the stream, before any beam's own events, so a consumer knows the shape of the run before watching it happen. |
| `beam_started` | `beam`. |
| `beam_cached` | `beam`, for a beam the cache answered instead of running. |
| `beam_output` | `beam`, `stream` (`stdout` or `stderr`), `text`, `replayed` (`true` for a line replayed from the cache rather than produced now). |
| `beam_finished` | `beam`, `status`, `exit_code` (`null` unless the beam ran and exited non-zero), `duration_ms`. |
| `run_finished` | `succeeded`, `cached`, `failed`, `failed_allowed`, `cancelled` (each a list of beam ids), `duration_ms`, and `exit_code`, what the run's beams earned. |

A watch session's stream carries three more kinds; see
[Watch mode](#watch-mode). No run summary is printed to stderr in this
format: `run_finished` already carries every count the text renderers
would have restated there.

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

On a terminal, where the interactive interface is the front end by default
(see [Interactive interface](#interactive-interface)), `q` exits with the
last run's own code, but only when that run reached its own end without
being abandoned; quitting mid-run, or before any run has finished, reports
`130` instead, since neither one vouches for the sources. Ctrl-C cancels a
run in flight without ending the session; with nothing running, it quits
the same way, `130`, even overriding a run that had just finished green.
Whatever keeps the interface from working at all, an unschedulable run or a
file watcher that fails to start, one that dies partway through, or the
session ending some other way than the user closing it, reports `2`, the
same as a headless session's own startup failures; so does `--ui` refused
because stdout is not a terminal.

### Executors

A beam runs its command on Alba's embedded shell by default (see
[Embedded shell](#embedded-shell) below); `executor system_shell` opts out
to the host shell instead, and `executor docker { image "..." }` runs it
inside a container (see [Docker executor](#docker-executor)). Any other
name, `executor <name> { ... }`, refers to an external plugin (see
[Plugins](#plugins)).

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

On a terminal, this session starts inside the interactive interface (see
[Interactive interface](#interactive-interface)) rather than printing the
text this section describes: the interface already lets `w` turn watching
on or off mid-session, so `--watch` there only chooses the state it starts
in. Everything below still describes the session itself: what counts as a
change, what a broken Beamfile does to it, how `--force` and debouncing
behave. It holds regardless of which front end is showing it; only the
paragraphs about clearing the screen and the JSON stream are specific to
the plain text renderers.

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

When the plain text renderers are the ones showing the session (off a
terminal, or forced on one by `--no-ui` or another flag that falls back to
them), they clear the screen before printing a triggered run whenever
stdout is a terminal, so each run starts on a clean page. This never
happens with `--log-format json`, and never happens when stdout is not a
terminal (a log file, a pipe): there is no screen to clear for a reader
parsing the stream, and off a terminal the output accumulates as a record
that clearing would destroy.

Piped through `--log-format json`, a session's stream carries three event
kinds beyond [an ordinary run's](#the-json-stream): `project_broken` (`diagnostic`, the same
rendered text also printed to stderr) whenever the project fails to load,
or a run cannot be scheduled once it has, `watch_waiting` (`files`, how many
resolved input files the session is watching) while it sits idle between
runs, and `watch_triggered` (`paths`, the changed paths that caused the next
run) the moment one starts. `paths` are relative to the project root, and
the array is empty when the trigger cannot be pinned to specific files,
such as a Beamfile that was broken and has just started parsing again. A
session parked on a project that will not load reports `project_broken`
once, then `watch_waiting` with `files` at `0` for as long as it stays
parked: it is stopped, not running, and nothing is resolved while nothing
loads.

Two things are worth knowing before leaning on watch mode. A beam that
writes to a git-tracked file matched by its own `inputs` triggers itself on
every run and loops forever, so keep generated output git-ignored. And
watch roots are computed once at startup, from the project root and the
directory of any Beamfile loaded from outside it: an import added
mid-session whose directory lies outside those roots is not watched until
the session is restarted.

## Interactive interface

On a terminal, `alba run` opens an interactive interface instead of
printing text: a live view of the run that takes over the whole screen for
as long as it lasts, and hands the terminal back exactly as it found it
once you quit (`q`).

Which front end a given invocation gets depends on stdout, not on typing
anything extra:

- It is the default whenever stdout is a terminal, unless `--log-format
  json`, an explicit `--output`, or `--no-ui` chooses the plain text
  renderers instead; all three win even together with `--ui`, since each
  already asks for a specific, script-readable shape of output the
  interface has nothing to add to.
- Off a terminal (piped, redirected, or under a test harness), with none
  of those three set, the text renderers run by default too, with no flag
  needed.
- `--ui` changes only that last case: instead of falling back quietly, it
  refuses with an error, since the interface still needs a real terminal
  to draw on, and drawing it down a pipe would just fill the reader's
  stream with escape codes.

The interface subsumes [watch mode](#watch-mode): `--watch` only chooses
the state the session starts in, and `w` toggles watching at any point
afterward. A Beamfile edit still reloads the project, a broken one still
parks the session until a later save fixes it, and a change still cancels
a run in flight and starts a fresh one, exactly as that section describes.

### Layout

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

The header and the bottom bar are not panes of their own: they sit inside
the top and bottom edge of a single outer frame, and one vertical divider,
part of that same frame, is what separates the tree from the log pane.

- **Header**: while a run is going, its progress bar, done/total, and
  elapsed time, ticking live; once it ends, the outcome by status (zero
  buckets omitted); between runs in a watch session, how many files it is
  watching; parked, instead, when the project cannot load.
- **Tree** (left, 30 columns): one row per beam, `✔` succeeded, `⚡`
  cached, `▶` running (its own duration ticking), `✖` failed (an allowed
  failure included), `○` pending or cancelled, plus a footer counting
  beams by status, with `▶` left out of the tally since it has not settled
  yet.
- **Logs** (right): the selected beam's output, following the tail by
  default. Scrolling up (the wheel, or the keys below) pauses following,
  so you can read in peace while the run continues; `G`, or scrolling back
  down to the bottom, resumes it.
- **Bottom bar**: the actions available in whatever mode is active, the
  keymap below condensed to what fits.

Below roughly 40 columns by 10 rows, the interface shows "terminal too
small" rather than a layout with nothing left to draw.

### Keymap

`Ctrl-C` behaves the same in every mode: it cancels a run in flight, or
quits if nothing is running.

**Normal**

| Key | Action |
| --- | --- |
| `q` | Quit. |
| `Ctrl-C` | Cancel the run in flight, or quit if idle. |
| `r` | Rerun the selected beam. |
| `f` | Rerun the selected beam, bypassing the cache. |
| `c` | Cancel the run in flight. |
| `w` | Toggle watch on or off. |
| `j`/`k`, `↓`/`↑` | Move the selection. |
| `t` | Run the target the session was started for. `r` and `f` retarget the session onto the beam they rerun, so this is the way back to the whole graph. |
| `PgUp`/`PgDn`, `Ctrl-u`/`Ctrl-d` | Scroll the log pane by half its height. |
| `G` | Jump the log pane to the tail and resume following. |
| Mouse wheel | Scroll the log pane. |
| `n` / `N` | Step the committed search forward or backward, wrapping; re-runs the query against the newly selected beam if the selection moved since it was committed. |
| `/` | Enter search. |
| `v` | Enter copy. |
| `g` | Enter graph. |
| `?` | Open help. |

**Search** (`/`)

| Key | Action |
| --- | --- |
| Any printable character | Add to the query (composing only, `n`/`N` included; stepping is the Normal-mode binding above). |
| `Backspace` | Erase the last character. |
| `Enter` | Commit the query, return to Normal, and keep the highlights. Committing is what gives `n`/`N` something to step. |
| `Esc` | Cancel and return to Normal: nothing is committed, the highlights go away, `n`/`N` have nothing to step, and the log pane resumes following the tail. |

**Copy** (`v`)

| Key | Action |
| --- | --- |
| `hjkl` / arrows | Move the cursor. |
| `v` | Re-anchor the selection at the cursor. |
| `y` | Copy the selection (OSC 52, falling back to the system clipboard when the terminal does not support it) and return to Normal; the bottom bar confirms for about two seconds. |
| `Esc` | Leave without copying. |
| Click and drag, then release | Select by dragging in the log pane; releasing copies. |

Copy is entered with `v`; a click-drag alone, without `v` first, does
nothing. Once inside, the selection addresses only the log pane's own
text; the tree beside it is never part of it.

**Graph** (`g`)

| Key | Action |
| --- | --- |
| `↑↓←→` | Move focus between nodes. |
| `Enter` | Select the focused beam and return to Normal. |
| `Esc` | Return to Normal without changing the selection. |

Graph mode draws the run's dependency graph in topological layers, colored
by status, and replaces the whole body while it is open: there is no tree
or log pane behind it.

**Help** (`?`)

| Key | Action |
| --- | --- |
| `?` or `Esc` | Close the overlay. |

Help draws the same keymap as this section, over the tree and log panes
dimmed rather than hidden.

What exit code the process reports once you quit is not a keymap
question; see [Exit codes](#exit-codes).

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

## Docker executor

`executor docker { image "..." }` runs a beam's commands inside a
container instead of on the host, via the `docker` CLI:

```
beam ship {
  executor docker {
    image "alpine:3"
    volumes ["/host/cache:/cache"]
    workdir "/srv"
  }
  run "./deploy.sh"
}
```

`image` is required; `volumes` (a list of `host:container` entries) and
`workdir` (an absolute path inside the container) are optional. Alba
starts one container per beam and keeps it alive, dormant, for every
command that beam's `run` list runs, so state a command leaves behind
(files written, anything a previous command set up) is visible to the
next one in the same beam; the container is removed once the beam ends.
The image must provide `/bin/sh`, since every command runs through
`docker exec ... sh -c "<command>"`.

The project directory is always bind-mounted into the container. On
unix it is mounted at its own host path, so a beam's working directory
needs no translation; on windows it is mounted at `/workspace` instead,
with the working directory rewritten to the matching path under
`/workspace`. A declared `workdir` always overrides that computed path.
`volumes` are bind-mounted the same way, in addition to the project
directory, and follow whatever mount syntax the `docker` CLI accepts for
a `-v host:container` argument.

Running a docker beam requires `docker` on the `PATH`; it reaches
whichever daemon that `docker` CLI is itself configured to talk to
(Docker Desktop, a remote context, an API-compatible lookalike).

## Plugins

`executor <name> { ... }` for any `name` that is not `shell`,
`system_shell`, or `docker` refers to an external plugin: a separate
`alba-executor-<name>` binary on the `PATH` that Alba spawns and speaks
a line-oriented JSON protocol to, one process per beam, kept alive for
every command in that beam the same way the docker executor keeps its
container alive.

Alba resolves every plugin a run needs before any beam starts: a beam
whose plugin cannot be found on the `PATH` fails the whole run
immediately, with a suggestion when the name looks like a typo of a
built-in executor:

```sh
$ alba run ghost
beam `ghost` uses executor `nosuchthing`: `nosuchthing` is neither a
built-in executor nor `alba-executor-nosuchthing` on the PATH
```

The full wire protocol — every message, its exact JSON shape, timeouts,
and cancellation — is specified in
[`docs/plugin-protocol.md`](docs/plugin-protocol.md). `crates/alba-executor-example`
is a complete reference implementation to read alongside it.

`alba plugin check <BINARY>` drives a plugin binary through that whole
protocol and reports whether it conforms, without needing a Beamfile:

```sh
$ alba plugin check target/debug/alba-executor-example
✓ handshake
✓ execute: exit code 0, 1 output line
✓ cancel (answered in 32.682583ms)
✓ close
conformant: protocol v1
```
