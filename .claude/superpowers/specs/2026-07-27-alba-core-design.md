# Alba — Core Design

Date: 2026-07-27 Status: approved

## Overview

Alba is a modern, high-performance task runner: a serious competitor to make, just, and task. Its manifest is the
`Beamfile`, written in a dedicated DSL. A **beam** is the unit of work, the equivalent of a make target.

Positioning: **task runner with intelligent caching**. A beam is a task (commands to run) with optional declared
`inputs`/`outputs`; caching and incremental execution build on fingerprinting those declarations (Turborepo/task
spirit), not on a make/bazel-style artifact freshness model. Simple to adopt; power comes progressively.

## Goals

- Parallel execution by default, bounded by a job limit.
- Intelligent caching and incremental runs (fingerprinting of declared inputs/outputs).
- Watch mode (re-run on file changes).
- Interactive TUI and first-class headless output (CI-friendly, JSON events).
- Excellent developer experience: readable DSL, Rust-quality error diagnostics, fast startup.
- Extensible executors: shell and docker built in; external plugins later through a JSON/stdio protocol (Terraform
  provider model).
- Git integration: affected-based execution, .gitignore-aware globs, git variables in the DSL, git hooks management.
- Monorepo support: Beamfile imports with namespaces.
- Cross-platform: macOS, Linux, Windows, with an embedded, home-grown POSIX-like shell interpreter as the default
  executor for identical behavior on all three platforms.

## Non-goals

- Being a full build system (no artifact freshness reasoning like make/bazel).
- A Turing-complete configuration language (no loops, user functions, or mutation in the DSL).
- A background daemon (single-process model; a daemon can be revisited later if profiling justifies it).
- Reimplementing full bash. The embedded shell targets a well-defined POSIX subset; advanced scripts opt out to a system
  shell.

## Key decisions

| Topic         | Decision                                                                                         |
|---------------|--------------------------------------------------------------------------------------------------|
| Language      | Rust                                                                                             |
| DSL           | Declarative + expressions; hand-written lexer and recursive descent parser                       |
| Architecture  | Cargo workspace, modular monolith, single binary, no daemon                                      |
| Async runtime | tokio                                                                                            |
| Executors     | Built-in behind an `Executor` trait; external process plugins (JSON/stdio) later                 |
| Default shell | Embedded home-grown POSIX-like interpreter (own sub-project); per-beam opt-out to a system shell |
| Platforms     | macOS + Linux + Windows from the start                                                           |
| Monorepo      | Explicit imports with namespaces (`api:build`)                                                   |
| License       | MIT (JDevelop)                                                                                   |

## Architecture

```
alba/
├── crates/
│   ├── alba-syntax      # lexer, parser, AST, diagnostics
│   ├── alba-core        # model (Beam, Project), import/namespace resolution, DAG
│   ├── alba-engine      # parallel scheduler, orchestration (later: cache, watch)
│   ├── alba-executors   # Executor trait + shell, docker implementations
│   ├── alba-shell       # embedded shell interpreter (later sub-project)
│   ├── alba-git         # git integration (later sub-project)
│   ├── alba-tui         # ratatui interface (later sub-project)
│   └── alba-cli         # binary: argument parsing, orchestration, headless output
└── Beamfile             # Alba builds itself from day one
```

Data flow: `alba-cli` receives `alba run build` → `alba-syntax` parses Beamfile (s) into an AST → `alba-core` evaluates
(variables, expressions, imports) into a validated `Project` with its beam DAG → `alba-engine`
computes the execution plan and schedules ready beams on a bounded parallel pool → each beam runs through its
`Executor` → execution events (started, output line, finished) flow through a single event channel consumed by the
headless renderer today and the TUI later.

Dependency rules:

1. Dependencies point downward only: `cli` → `engine` → `core` → `syntax`.
2. `alba-engine` knows executors only through the `Executor` trait. This is what will make the external plugin protocol
   trivial to add.
3. The event channel is the only contract between execution and display; headless output and the TUI are two consumers
   of the same stream.

## The Beamfile DSL

Principles: readable in 30 seconds, declarative, brace-delimited blocks (no significant whitespace), Rust-quality
diagnostics (spans, suggestions). Not Turing-complete: no loops, no user-defined functions, no mutation. Complex logic
belongs in scripts invoked by `run`.

```text
version "1"

import "api/Beamfile" as api

let profile = env("PROFILE", "debug")
let release = profile == "release"

default build

beam build {
  description "Compile the workspace"
  needs [api:build, codegen]
  inputs ["src/**/*.rs", "Cargo.toml"]
  outputs ["target/{profile}/app"]
  run "cargo build {if release then '--release' else ''}"
}

beam test {
  needs [build]
  run "cargo test"
}

beam lint {
  allow_failure true
  run "cargo clippy -- -D warnings"
}

beam deploy(target) {
  description "Deploy to an environment"
  needs [test]
  executor docker { image "deployer:latest" }
  env { DEPLOY_TARGET = target }
  run "./scripts/deploy.sh {target}"
}
```

Language constructs (exhaustive):

- `beam name(params) { ... }` — the unit of work. Optional parameters are passed from the CLI:
  `alba run deploy staging`.
- Beam fields: `description`, `needs` (dependencies, namespaced or local), `inputs` / `outputs` (globs, consumed by the
  cache),
  `run` (a string for a single command, or a `[...]` list of strings executed sequentially, stopping at the first
  failure), `env`, `cwd`,
  `executor`,
  `allow_failure` (default `false`), later `watch`.
- `let` — immutable file-level variables with expressions (`==`, `&&`,
  `if/then/else`, string concatenation) and built-in functions (`env()`,
  `glob()`, later `git.branch` and friends).
- `import "path" as ns` — monorepo composition; imported beams are namespaced `ns:`.
- `default <beam>` — the beam run by a bare `alba` / `alba run`. At most one per root Beamfile; `default` declarations
  in imported Beamfiles are ignored (only the root decides). Without a `default`, a bare `alba`
  lists beams with their descriptions.
- Interpolation `{expr}` in all strings; `{{` escapes a literal brace.

`allow_failure` semantics: on failure the beam is reported as
`failed (allowed)`, its dependents still run, and the failure does not affect the run's exit code.

Implementation: hand-written lexer and recursive descent parser (no parser generator — best error messages and full
control), typed AST, diagnostics with spans in the style of miette/ariadne.

## Execution engine

Plan construction: `alba-core` validates the whole graph at load time — unknown beams in `needs`, cycles (reported with
the exact path, e.g.
`build → codegen → build`), missing parameters. `alba run test` then extracts the transitive subgraph of `test`; only
what is needed is scheduled.

Scheduler: tokio runtime. Each beam becomes an async task that awaits completion of its `needs`, then executes as soon
as a slot is free. Parallelism is bounded by a semaphore: `--jobs N`, defaulting to the number of CPU cores. Child
processes are spawned asynchronously with stdout/stderr streamed line by line.

Failure handling: fail-fast by default — a failing beam (unless
`allow_failure`) cancels not-yet-started beams, lets running ones finish, then prints a summary: succeeded, failed,
cancelled. `--keep-going` runs everything still possible. Exit codes: 0 success, 1 beam failure, 2 Alba error (parsing,
configuration, ...).

Headless output: every line prefixed with the colorized beam name (`api:build │ Compiling...`), final summary with
per-beam durations.
`--output interleaved|grouped` (grouped buffers and prints per finished beam; more readable in CI). TTY detection
disables colors and animations in CI. `--log-format json` emits one JSON line per event, dogfooding the event channel.

Executor contract:

```rust
#[async_trait]
trait Executor {
    async fn execute(&self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult>;
}
```

`CommandSpec` carries the command, environment, and working directory.
`ExecContext` carries the event channel and a cancellation token. Ctrl-C propagates cancellation cleanly: SIGTERM, grace
period, SIGKILL.

## Sub-project roadmap

Each sub-project gets its own spec → plan → implementation cycle.

1. **Core** (this spec's implementation scope): DSL, model, DAG, parallel scheduler, shell executor (system shell
   provisionally), headless CLI.
2. **Cache / incremental**: fingerprinting of inputs/outputs, skip of unchanged beams, .gitignore-aware globs, git
   variables in the DSL.
3. **Embedded shell**: the home-grown POSIX-like interpreter (`alba-shell`), becoming the default executor.
4. **Watch mode**: re-run on file changes.
5. **TUI**: ratatui interface — beam tree, per-beam logs, live statuses.
6. **Docker executor + plugin protocol**: docker execution and the external executor protocol (JSON/stdio).
7. **Affected + git hooks**: `alba run --affected <ref>` and git hooks management (husky/lefthook spirit).

## Sub-project 1 scope (Core)

Deliverables:

- Crates `alba-syntax`, `alba-core`, `alba-engine`, `alba-executors`
  (system shell only), `alba-cli`.
- The full DSL as specified above, with two deliberate freezes:
  `inputs` / `outputs` are parsed and validated but inert (the cache lands in sub-project 2; the syntax is frozen now so
  Beamfiles never break), and `executor docker { ... }` parses but returns a clear
  "not yet supported" error.
- CLI: `alba` (list or default beam), `alba run <beam> [args]`,
  `--jobs`, `--keep-going`, `--output`, `--log-format json`,
  `alba check` (validate the Beamfile without executing).
- Provisional shell executor: `sh -c` on Unix, PowerShell on Windows, behind the `Executor` trait. The embedded
  interpreter replaces it as the default in sub-project 3 without touching the engine.

## Testing strategy

TDD (red, green, refactor) throughout.

- `alba-syntax`: unit tests for lexer and parser + insta snapshots of ASTs **and of error messages** (diagnostics are a
  product feature; they get tested).
- `alba-core`: resolution tests — imports, namespaces, cycle detection, interpolation, parameters.
- `alba-engine`: scheduler tested against a `FakeExecutor` (the trait earns its keep from day one) — topological order
  respected, effective parallelism, fail-fast, cancellation, `allow_failure`.
- `alba-cli`: end-to-end integration tests with real Beamfiles in temporary directories (`assert_cmd`).

## Success criteria (sub-project 1)

- Alba replaces just/make for daily use on the author's own projects.
- The Alba repository builds itself through its own `Beamfile`.
- Parsing + planning under 10 ms on a typical Beamfile.
- CI green on macOS, Linux, and Windows.
