# Alba — Docker Executor and Plugin Protocol

Date: 2026-08-06 Status: approved

## Overview

Sub-project 6 of the [core design](2026-07-27-alba-core-design.md): docker execution and the external executor
protocol. `executor docker { ... }` stops being a parse-only stub and runs beams inside containers, and any
unknown executor name becomes a reference to an external plugin binary spoken to over JSON/stdio — the Terraform
provider model, adapted to executors.

Both ride on one architectural change: the `Executor` contract grows a **per-beam session**. Today the engine
calls `execute` once per `run` entry with no notion of a beam beginning or ending; a container (or a plugin
process) needs exactly that lifecycle. The session gives beam-scoped state a clear owner while the engine stays
the sole owner of sequencing, and the plugin protocol falls out of the trait as a direct wire transcription.

Docker itself stays a built-in, native executor (single-binary promise of the core design); the protocol is
proven by a maintained example plugin and a conformance subcommand instead.

## Goals

- Run a beam's commands inside a container from a declared image, with the project mounted and the beam's
  `env`/`cwd` semantics preserved.
- Extend the docker DSL block: `image` (required), `volumes`, `workdir` (optional).
- Define and implement protocol v1 for external executors: discovery by PATH convention, JSON Lines over
  stdin/stdout, per-beam process lifecycle, cancellation, robust failure handling.
- Ship a conformance tool (`alba plugin check <binary>`) and an in-repository example plugin.
- Rework the `Executor` trait into `Executor::open → ExecSession` with guaranteed cleanup, migrating the
  existing executors as stateless sessions with no observable behavior change.

## Non-goals

- Plugin registry, installation, or version pinning — the PATH is the distribution mechanism.
- Non-executor plugins (cache backends, notifiers, ...).
- Advanced docker options: `user`, `network`, `platform`, pull policy. The block syntax accommodates them later
  without breakage.
- A native podman executor — a plugin can provide it.
- Docker as a plugin: it is built in.

## Key decisions

| Topic              | Decision                                                                                   |
|--------------------|--------------------------------------------------------------------------------------------|
| Docker integration | Built-in native executor, shelling out to the `docker` CLI (no API client dependency)      |
| Container lifetime | One container per beam: started at session open, commands via `docker exec`, removed at close |
| Executor contract  | Two-level: `Executor::open(BeamContext) -> ExecSession`; engine keeps sequencing and guarantees `close` |
| Executor options   | Carried as JSON (`serde_json::Value`) in `BeamContext` — keeps `alba-executors` free of `alba-core` |
| Plugin discovery   | PATH convention: `executor foo { ... }` → binary `alba-executor-foo`, resolved at plan time |
| Plugin transport   | One process per session; JSON Lines on stdin/stdout; plugin stderr relayed as beam stderr  |
| Protocol version   | Single integer `protocol: 1` in the open message; unsupported → plugin answers `error`     |
| Conformance        | `alba plugin check <binary>` subcommand + example plugin crate in the workspace            |

## The session contract

`alba-executors` replaces the single-shot trait with:

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError>;
}

#[async_trait]
pub trait ExecSession: Send {
    async fn execute(&mut self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError>;
    async fn close(self: Box<Self>) -> Result<(), ExecError>;
}
```

`BeamContext` carries: a human-readable beam label (names containers and plugin processes), the beam's
directory, an output channel (session setup can produce lines — an image pull, for example), a cancellation
token, and the executor options as `serde_json::Value`. The JSON is what keeps the trait uniform without a
dependency on `alba-core`: the engine serializes `ExecutorKind` into it, the docker session deserializes it into
its typed configuration, and the plugin session forwards it verbatim to the external binary.

Engine rules:

- The engine opens the session lazily (when the beam's first command is about to run), calls `execute` once per
  `run` entry, stops at the first failure, and calls `close` **in every case** — success, failure, and
  cancellation. Cleanup is guaranteed by the engine, not by executor goodwill.
- The embedded and system shell executors become trivial stateless sessions: `open` returns immediately,
  `close` does nothing. No observable behavior change.
- `FakeExecutor` follows the same contract so scheduler tests can assert `open → execute* → close` ordering and
  cleanup on cancellation.
- The executor label that participates in the cache key (today `"embedded"`/`"system"`, a `&'static str`)
  becomes a string incorporating the executor configuration: changing the docker image, a volume, or a plugin
  option invalidates the beam's cache entry.

## The docker executor

Implementation: subprocess calls to the `docker` CLI. No API client dependency; automatic compatibility with
Docker Desktop, docker contexts, and CLI-compatible lookalikes. The binary is resolved on the PATH at session
open; if absent, `open` fails with a clear diagnostic.

Session lifecycle:

- `open`: `docker run -d` of a container kept alive by a dormant process through the image's shell (a sleep
  loop via `sh -c`). Documented prerequisite: **the image must provide `/bin/sh`** — the same assumption CI
  runners make. The container is named after the beam (`alba-` prefix, random suffix) and labeled
  (`alba.beam=...`) so it is identifiable in `docker ps`. A missing image triggers `docker run`'s implicit
  pull; its progress is relayed through the beam's output channel.
- `execute`: `docker exec` with `-w` for the working directory and one `-e` per `CommandSpec` environment
  variable, the command passed to `sh -c`. Stdout/stderr are streamed line by line as today; the command's exit
  code is `docker exec`'s.
- `close`: `docker rm -f`. The container also runs with `--rm`, so even an Alba crash leaves cleanup to the
  docker daemon once the dormant process dies.

Mounts and paths:

- On unix, the project root (the root Beamfile's directory) is mounted **at the same absolute path** inside the
  container, and each command's `cwd` applies verbatim — no mapping, paths printed by tools remain valid on the
  host.
- On windows (linux containers), identical paths are impossible: the project root is mounted at `/workspace`
  and `cwd` values are rewritten relative to it.
- `volumes ["host:container"]` adds extra mounts (caches, sockets); `workdir "/path"` forces the default
  working directory instead of the computed `cwd`.

Cancellation: during `open`, the in-flight `docker run` (including a long pull) is killed; during `execute`, the
container is stopped with `docker stop` (SIGTERM plus a short grace period) and then removed, which terminates
the `exec` — consistent with the other executors' cancellation semantics.

Model side: `ExecutorKind::Docker` grows to `{ image, volumes, workdir }`, the last two optional, validated at
evaluation time (non-empty `host:container` form; absolute `workdir`).

## The plugin protocol

Resolution: an executor name that is neither `shell`, `system_shell`, nor `docker` becomes
`ExecutorKind::Plugin { name, options }`. At plan time (where docker rejection lives today) the engine looks for
`alba-executor-<name>` on the PATH; a missing binary fails the run cleanly before anything starts, with a
diagnostic naming what was searched and suggesting a likely typo ("`dokcer` is neither a built-in executor nor
`alba-executor-dokcer` on the PATH — did you mean `docker`?"). The options block accepts arbitrary keys
(strings, booleans, lists of strings, with interpolation), forwarded verbatim — validating them is the plugin's
contract, not Alba's.

Transport: one plugin process per session (hence per beam), speaking JSON Lines — one JSON object per line — on
stdin/stdout. The plugin's stderr is relayed into the beam's output as stderr lines: authors debug with plain
eprintln. The protocol transcribes the session trait:

```
Alba → plugin   {"type":"open","protocol":1,"beam":"deploy","dir":"/abs/path","options":{...}}
plugin → Alba   {"type":"ready"}                          or {"type":"error","message":"..."}

Alba → plugin   {"type":"execute","command":"...","env":[["K","V"]],"cwd":"/abs/path"}
plugin → Alba   {"type":"output","stream":"stdout","text":"..."}   (zero or more)
plugin → Alba   {"type":"exit","code":0}                  or {"type":"error","message":"..."}

Alba → plugin   {"type":"close"}
plugin          exits (exit code ignored)
```

Versioning: `protocol: 1` in the open message; a plugin that does not support it answers `error` with an
explicit message. A single integer, no negotiation — negotiation arrives if a protocol 2 ever exists.

Cancellation: Alba sends `{"type":"cancel"}` during an `execute`; the plugin must interrupt the running command
and answer with its `exit` (the code reflecting the interruption). A deaf plugin is killed after the grace
period — the same SIGTERM/grace/SIGKILL ladder as the other executors.

Robustness: a 10-second timeout on the handshake (`open` → `ready`); a non-JSON line or an unexpected message type maps
to an `ExecError` quoting the offending line; a prematurely dead process maps to an `ExecError` carrying the
plugin's exit code. All of these are beam failures, never Alba panics.

Deliverables: the protocol specification published as `docs/plugin-protocol.md` (product documentation), an
example plugin in the repository (a tiny Rust binary crate, used by the end-to-end tests), and conformance
tooling as **`alba plugin check <binary>`**: a subcommand that drives any binary through the protocol
(handshake, execution, output, cancellation, errors) and prints a conformance report — usable by a plugin
author in any language, with no Rust toolchain.

## DSL changes

Both backward compatible:

- The docker block accepts `volumes` (list of `"host:container"` strings) and `workdir` (string), optional,
  alongside the required `image`. Interpolation allowed in all three.
- An unknown executor name stops being an evaluation error ("did you mean `docker`?"): it becomes
  `ExecutorKind::Plugin { name, options }`, existence checked at plan time. `alba check` keeps validating
  syntax and model without touching the PATH — a Beamfile validates on a machine where its plugins are not
  installed.

## Error handling

Three families, all run-level failures, never panics:

- Session open failures — docker missing from the PATH, image not found, plugin binary missing, failed
  handshake: the beam fails with the exact cause.
- Protocol violations — invalid JSON, unexpected message, dead plugin: the beam fails with the offending line
  quoted.
- The scheduler's docker `unreachable!` arms disappear, replaced by real dispatch; the current plan-time docker
  rejection disappears too. The only remaining plan-time executor error is an unresolvable plugin.

## Testing strategy

TDD (red, green, refactor) throughout, as in previous sub-projects.

- `alba-executors`: unit tests of the session contract on the existing executors (trivial sessions, no behavior
  change); the plugin executor tested against scripted fake plugins — test binaries speaking the protocol
  (well-behaved, mute, invalid-JSON-babbling, cancel-deaf); docker tested in integration behind a guard
  (skipped without a docker daemon, enabled in linux CI).
- `alba-engine`: `FakeExecutor` migrated to the session contract; scheduler tests for `open → execute* → close`
  ordering, `close` guaranteed on failure and cancellation, and the cache label incorporating docker
  configuration (image change → invalidation).
- `alba-cli`: end-to-end with the workspace-built example plugin (a real Beamfile using it); `alba plugin
  check` tested against the example plugin (conformant) and against a broken fake (failure report); docker
  end-to-end guarded as above.
- CI: the linux job gains the docker step; macOS and windows do not run docker tests (no daemon on the runners)
  but compile and test everything else, plugins included.

## Success criteria

- A beam with `executor docker { image "..." }` runs its commands in a container with the project mounted, on a
  machine with docker installed, with correct output streaming, exit codes, and cancellation.
- A third-party binary following `docs/plugin-protocol.md` passes `alba plugin check` and runs beams end to end
  with no change to Alba.
- The existing executors are untouched observably: the full pre-existing test suite passes after the session
  migration.
- CI green on macOS, linux, and windows.
