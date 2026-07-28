# Alba — Embedded Shell Design

Date: 2026-07-28 Status: approved

## Overview

Sub-project 3 of the [core design](2026-07-27-alba-core-design.md): the embedded shell interpreter (`alba-shell`).
It replaces `sh -c` / PowerShell as Alba's default executor so that the same `run` command behaves identically on
macOS, Linux, and Windows. The interpreter is home-grown (lexer, recursive descent parser, async tree-walking
interpreter) and ships with a small set of built-in cross-platform utilities, because a grammar alone cannot make
`rm -rf dist && mkdir dist` portable. Everything sits behind the existing `Executor` trait: the engine, the event
channel, the cache, and the renderers do not change.

## Goals

- One deterministic, POSIX-like semantics for `run` commands on all three platforms.
- Built-in utilities (`rm`, `cp`, `mkdir`, ...) so common file operations work identically everywhere.
- Rust-quality diagnostics: spans inside the command string, plain-language messages, actionable suggestions.
- Static validation: `alba check` parses statically known commands without executing anything.
- A clean opt-out per beam (`executor system_shell`) for scripts beyond the supported subset.

## Non-goals

- Full POSIX conformance or bash compatibility. Where POSIX leaves behavior implementation-defined, we freeze one
  behavior, identical on all platforms.
- Shell control flow (`if`, `for`, `while`, `case`), functions, heredocs. Complex logic belongs in scripts invoked
  by `run`, or behind the system-shell opt-out.
- Background jobs (`&`, `wait`). Parallelism is the engine's job, not a beam command's.
- Automatic fallback to a system shell on unsupported syntax. Out-of-subset syntax is a clear error with a
  suggestion, never a silent platform-dependent re-execution.
- Interactivity. Stdin stays null, as it is today.
- Windows Job objects. The shell kills the processes it spawned itself; descendants that double-fork or daemonize
  can still escape on Windows, exactly as documented for the current executor.

## Key decisions

| Topic               | Decision                                                                               |
|---------------------|----------------------------------------------------------------------------------------|
| Implementation      | Home-grown lexer, recursive descent parser, async tree-walking interpreter (tokio)     |
| Built-in utilities  | Yes, 16 builtins with a deliberately minimal, documented flag surface                  |
| Builtin precedence  | A builtin always wins over a PATH binary of the same name; explicit paths bypass it    |
| Control flow        | Not supported; clear error plus suggestion                                             |
| Opt-out             | `executor system_shell` on the beam (existing `executor` field, declaration, no block) |
| Unsupported syntax  | Deterministic error with span and suggestion; no automatic fallback                    |
| Validation timing   | `alba check` parses statically known commands; every rendered command parses pre-spawn |
| Crate position      | `alba-shell` is standalone, no dependency on any other Alba crate                      |
| Non-core primitives | `glob` for globbing, `which` for PATH resolution, `os_pipe`/tokio for pipes           |

## Supported language

- Sequencing: `cmd1 ; cmd2`, `cmd1 && cmd2`, `cmd1 || cmd2`, negation `! cmd`.
- Pipelines: `cmd1 | cmd2`. Exit code is the last command's (POSIX default; no `pipefail`).
- Redirections: `>`, `>>`, `<`, `2>`, `2>>`, `2>&1`.
- POSIX quoting: single quotes (literal), double quotes (expansions apply), backslash escapes.
- Expansions: `$VAR`, `${VAR}`, command substitution `$(...)`, tilde `~` at the start of a word, globbing `*`,
  `?`, `[...]` via `glob`. Globs resolve against the raw disk; no `.gitignore` filtering (this is a shell, not
  the cache).
- Variables: shell assignment `FOO=bar` (scoped to the current `run` line), environment prefix `FOO=bar cmd`, and
  the `export` / `unset` builtins. Each entry of a `run [...]` list is an independent shell invocation: variables
  do not persist across entries. A beam's durable environment is the DSL `env` field.

DSL interpolation (`{expr}`) remains a textual substitution performed upstream by the engine, before the shell
ever sees the command, exactly as today.

Out of subset, rejected with a clear diagnostic: control flow, functions, heredocs, `&` and `wait`, subshells
`(...)`, `set` and shell options, advanced expansions (`${VAR:-default}`, arithmetic `$((...))`, brace expansion
`{a,b}`).

## Architecture

`alba-shell` is a new standalone crate at the bottom of the dependency graph, like `alba-executors`: it depends on
no other Alba crate. Public API:

```rust
pub fn parse(source: &str) -> Result<Program, ShellParseError>;
pub async fn execute(program: &Program, env: ShellEnv) -> ShellResult;
```

- `ShellParseError` carries the span inside the source string, the message, and an optional suggestion.
- `ShellEnv` carries the environment variables, the working directory, the output line sender, and the
  cancellation token: the same ingredients as today's `CommandSpec` plus `ExecContext`.
- `ShellResult` carries the exit code.

Integration points:

- `alba-executors` gains `EmbeddedShellExecutor`, the new default. It implements the unchanged `Executor` trait:
  parse the rendered command, then execute. A parse error becomes a beam failure with the full diagnostic; no
  process is ever spawned for an invalid command.
- `SystemShellExecutor` stays as is and becomes the opt-out behind `executor system_shell` in the DSL (a
  declaration without a block, next to the already-parsed `executor docker { ... }`).
- `alba check` parses the command of every beam targeting the embedded shell whose command is statically known
  (no unsupplied parameters), and reports errors as ordinary Alba diagnostics with the offending beam's name.
- The engine, the event channel, the scheduler, the cache, and the renderers change in no way.

## Execution model

- A pipeline stage is either an external process or an in-process builtin; both connect through OS pipes, a
  builtin reading and writing virtualized streams the way a process would.
- The shell spawns every external process itself: PATH resolution via `which` (`.exe`, `.cmd`, `.bat` on
  Windows), per-stage environment and working directory.
- The pipeline's final stdout and every stage's stderr are split into lines and sent through the output sender,
  preserving today's contract (stdout and stderr lines may interleave in any relative order).
- Command substitution `$(...)` captures stdout in memory, trailing newline stripped, no size cap in v1.
- Cancellation: the interpreter checks the token between commands. Each external process is terminated
  individually: process-group SIGTERM, grace period, SIGKILL on unix, as today. On Windows the shell kills each
  child it spawned itself, which improves on the current situation (no intermediate PowerShell process), without
  reaching a child's own descendants (Job objects are out of scope).

## Builtins

Sixteen builtins, each with a deliberately minimal, documented flag surface. An unsupported flag is a clear
error, never a silently different behavior.

- Pure shell: `cd`, `pwd`, `exit`, `true`, `false`, `export`, `unset`
- Output: `echo` (only flag: `-n`; no escape interpretation; the classic bash/sh divergence is frozen)
- Files: `cat`, `cp` (`-r`), `mv`, `rm` (`-r`, `-f`), `mkdir` (`-p`), `touch`
- Utilities: `sleep` (seconds, decimals accepted), `test` / `[` (`-f`, `-d`, `-e`, `-z`, `-n`, `=`, `!=`, `-eq`,
  `-ne`, `-lt`, `-le`, `-gt`, `-ge`)

A builtin always takes precedence over a PATH binary of the same name; that determinism is the point. A system
binary remains reachable through an explicit path (`/bin/echo`).

## Diagnostics

Same standard as the DSL, three families:

- Syntax errors: for example an unclosed quote, with the span pointing at the opening quote.
- Out-of-subset syntax: named construct, span, and the suggestion to move the logic into a script or declare
  `executor system_shell`.
- Execution errors: command not found (with a did-you-mean against builtins and PATH when the edit distance is
  close), unreadable redirection target, and similar.

Through `alba check` these render in Alba's standard diagnostic output, tagged with the beam name.

## Testing strategy

TDD (red, green, refactor) throughout.

- Lexer and parser: unit tests plus insta snapshots of ASTs and of error messages (diagnostics are a product
  feature; they get tested).
- Interpreter: a conformance suite of pairs, command line to expected output and exit code, executed identically
  on all three platforms with no `cfg` in the expectations. This suite embodies the cross-platform promise.
- Builtins: pure tests without processes where possible; pipelines and redirections tested in temporary
  directories with real processes.
- Cancellation: termination tests (external process killed, builtin `sleep` interrupted, whole pipeline stopped).
- Integration: `EmbeddedShellExecutor` behind the trait; CLI end-to-end tests with real Beamfiles; the existing
  engine tests (FakeExecutor) do not move.

## Success criteria

- Alba's own Beamfile runs on the embedded shell by default, CI green on macOS, Linux, and Windows.
- The conformance suite passes identically on the three platforms.
- Parsing a typical command takes under 1 ms (the core spec's parsing plus planning budget of 10 ms holds).
- `executor system_shell` works as the documented opt-out.
