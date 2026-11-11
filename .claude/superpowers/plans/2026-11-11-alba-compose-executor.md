# Compose Executor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for
> tracking.

**Goal:** `executor compose { service "php" }` runs a beam's commands in a one-off container of a Compose
service, with `compose.yaml` as the only description of that container.

**Architecture:** A new `compose` module in `alba-executors` shells out to `docker compose`: it reads the
service with `config`, builds it, starts its dependencies with `up -d --no-recreate --wait`, then holds one
`run` container per beam alive through a stdin pipe and runs commands in it with `docker exec`. The `Executor`
trait gains a `shutdown` method the engine calls when the command ends, which is where the dependencies Alba
started go back to their previous state. The code both container executors need moves into a shared
`container` module.

**Tech Stack:** Rust (workspace edition and toolchain as pinned), tokio, serde_json, the `docker` CLI with the
Compose plugin.

**Spec:** `.claude/superpowers/specs/2026-11-10-alba-compose-executor-design.md`

## Global Constraints

- Shell out to `docker compose`; no API client.
- Never issue `docker compose down`, `--remove-orphans`, `--force-recreate`, or `-v`/`--volumes` on a removal,
  outside test teardown of a test's own throwaway project.
- The compose executor never passes `--user`, `-e`, `-v`, `-w`, or `--service-ports` to `docker compose run`.
- No regression on the docker executor or the plugin protocol: their existing tests pass unchanged, apart
  from tests that move with the code they cover in Task 3.
- `ExecSession` and `docs/plugin-protocol.md` are not modified.
- `alba check` never runs `docker`.
- Every `docker compose` invocation carries the global flag `--progress quiet`, runs with the beam's directory
  as its working directory, and carries `-f <beam.dir joined with file>` when `file` is set.
- Tests against a daemon are `#[ignore = "requires a running docker daemon"]`, use `alpine:3`, and a Compose
  project name unique to the test and process.
- Tests follow the repository's existing pattern: evaluation and pure functions under inline `#[cfg(test)]`
  modules, engine behaviour through `alba_engine::run`/`session` with `FakeExecutor`, executors through
  `Executor::open` against a real daemon.
- Commits: gitmoji + Conventional Commits, scope by subject, no reference to this plan or its task numbers.
  No `Co-Authored-By` trailer.
- `CHANGELOG.md` is generated from commit subjects at release: do not edit it by hand.
- Run `alba run check` before each commit (it is also the pre-push hook).

## Facts verified against Docker Compose v5.6.1

The implementer can rely on these; they were measured, not assumed.

- `docker compose config --format json <service>` returns `name` and, under `services`, the target service
  and its transitive dependencies only. It works for a service behind an inactive profile. Each service has
  `depends_on` as an object keyed by service name (absent when there is none), `volumes` as a list of
  `{"type": "bind" | "volume", "source": ..., "target": ...}` with absolute, symlink-resolved bind sources,
  and a `build` key only when the service declares one. An unknown service exits 1 with
  `no such service: <name>` on stderr.
- `docker compose ps -a --format json` prints one JSON object per line with `Service` and `State`
  (`running`, `exited`, `created`, ...). A service with no container has no line.
- `docker compose --progress quiet build <service>` prints nothing when it succeeds and the build log on
  stderr when it fails.
- `docker compose --progress quiet up -d --no-recreate --wait <services>` leaves a running service untouched
  even when its definition diverged, starts absent and stopped ones, waits for `service_healthy`, and
  succeeds for a one-shot service that exited 0.
- `docker compose rm -s -f <services>` prints `Going to remove ...` on stdout.
- A `run` container started with `sh` on a held stdin pipe executes `echo <token>` written to that pipe and
  prints the token on stdout, about 400 ms after the `run` was spawned.

## Review Focus

1. A beam whose directory is reached through a symlink (`/tmp` on macOS): the working directory still maps,
   because both sides are compared in canonical form. Test in Task 4.
2. A `cwd` no bind mount of the service covers (a beam's `cwd` override, or a service with only named
   volumes): the command fails naming the `cwd`, the service, and its bind mounts. Tests in Tasks 4 and 5.
3. A `service` value starting with `-` (`service "--help"`): rejected at evaluation, never handed to the
   `docker` command line. Test in Task 7.
4. A service whose image has no `sh`: the beam fails with a message naming the service, and no container is
   left behind. Test in Task 5.
5. A dependency that cannot start (an image that does not exist): the beam fails with Compose's message, and
   `shutdown` still puts back the dependencies that did start. Test in Task 6.

---

## File structure

| File                                           | Responsibility                                                         |
| ---------------------------------------------- | ---------------------------------------------------------------------- |
| `crates/alba-executors/src/lib.rs`             | `Executor::shutdown`; module declarations and re-exports               |
| `crates/alba-executors/src/fake.rs`            | `FakeExecutor` records and can fail `shutdown`                         |
| `crates/alba-executors/src/container.rs`       | New. `docker exec`, naming, removal: what both container executors use |
| `crates/alba-executors/src/docker.rs`          | Loses what moved to `container.rs`                                     |
| `crates/alba-executors/src/compose/mod.rs`     | New. `ComposeExecutor`, its session, the `docker compose` invocations  |
| `crates/alba-executors/src/compose/service.rs` | New. Parses `config` output; maps a host directory to a container path |
| `crates/alba-executors/src/compose/started.rs` | New. Which dependencies Alba started, and their previous state         |
| `crates/alba-executors/tests/compose.rs`       | New. Daemon tests of the executor                                      |
| `crates/alba-core/src/model.rs`, `eval.rs`     | `ExecutorKind::Compose` and its evaluation                             |
| `crates/alba-engine/src/scheduler.rs`          | `Executors::compose`, `Executors::shutdown`, label, options, `run`     |
| `crates/alba-engine/src/event.rs`              | `RunSummary::cleanup_errors`                                           |
| `crates/alba-engine/src/session.rs`, `watch/`  | `SessionEnd`; shutdown when the session ends                           |
| `crates/alba/src/commands/run.rs`              | Wires `ComposeExecutor`; reports cleanup errors                        |
| `crates/alba/src/render/*.rs`                  | Render `cleanup_errors`                                                |
| `crates/alba/tests/cli_compose.rs`             | New. Daemon tests through the binary                                   |

---

### Task 1: `Executor::shutdown`, called at the end of a plain run

**Files:**

- Modify: `crates/alba-executors/src/lib.rs`, `crates/alba-executors/src/fake.rs`
- Modify: `crates/alba-engine/src/scheduler.rs`, `crates/alba-engine/src/event.rs`,
  `crates/alba-engine/src/session.rs`
- Modify: `crates/alba/src/render/grouped.rs`, `interleaved.rs`, `json.rs`; `README.md` ("The JSON stream")
- Test: `crates/alba-engine/tests/scheduler.rs`, inline tests of the three renderers

**Interfaces:**

- Produces, in `alba-executors`:
  - `Executor::shutdown(&self) -> Result<(), ExecError>`, default body `Ok(())`.
  - `FakeEvent::ShutDown`, pushed to the fake's event list on every `shutdown` call.
  - `FakeExecutor::fail_shutdown(self, message: impl Into<String>) -> Self`: `shutdown` then returns that
    message as its error (and still records `ShutDown`).
- Produces, in `alba-engine`:
  - `Executors::shutdown(&self) -> Vec<String>`: calls `shutdown` once per distinct executor among its slots
    (`Arc::ptr_eq`), every slot attempted, returns the error messages in slot order.
  - `RunSummary::cleanup_errors: Vec<String>`; `RunSummary::exit_code()` returns 1 when it is non-empty.
  - `scheduler::run`: unchanged signature. After every beam task is awaited it calls `executors.shutdown()`,
    stores the result in the summary, then emits `RunFinished` (still last). On its `Err` paths it also calls
    `executors.shutdown()` and drops the result: the `EngineError` is what gets reported.
  - `pub(crate) async fn run_in_session(...)`, same parameters and return type as `run`, never shuts down.
    `session.rs` calls it instead of `run`.
- Plugin executors are built per run inside the scheduler and keep the default no-op: they are not part of
  `Executors` and are not shut down.

- [ ] **Step 1: Write the failing engine tests** in `crates/alba-engine/tests/scheduler.rs`, with the file's
      existing helpers (`run_target`, `run_target_with_cancel`, `run_target_with_executors`):

  - `the_executors_shut_down_once_after_every_session_closed`: one beam, two commands. The fake's events end
    with `[.., Closed { beam: "build" }, ShutDown]` and contain exactly one `ShutDown`.
  - `the_executors_shut_down_when_a_beam_fails`: a command exits 1; exactly one `ShutDown`, last.
  - `the_executors_shut_down_when_the_run_is_cancelled`: cancel mid-command; exactly one `ShutDown`, after the
    `Closed`.
  - `an_executor_filling_several_slots_shuts_down_once`: `Executors::uniform(fake)`; exactly one `ShutDown`.
  - `a_shutdown_failure_fails_a_run_whose_beams_all_passed`:
    `FakeExecutor::new().fail_shutdown("db would not stop")`; the returned summary has
    `failed.is_empty()`, `cleanup_errors == ["db would not stop"]`, `exit_code() == 1`, and the `RunFinished`
    event carries the same `cleanup_errors`.

- [ ] **Step 2: Run them, expect failure**

  Run: `cargo test -p alba-engine --test scheduler shut` Expected: does not compile (`FakeEvent::ShutDown`,
  `fail_shutdown`, `cleanup_errors` unknown).

- [ ] **Step 3: Implement** the interfaces above.

- [ ] **Step 4: Run them, expect success**

  Run: `cargo test -p alba-engine` Expected: PASS, including the pre-existing scheduler and watch tests.

- [ ] **Step 5: Write the failing renderer tests**, inline, next to each renderer's existing `RunFinished`
      test, for a summary with `cleanup_errors: vec!["db would not stop".into()]`:

  - `grouped.rs` and `interleaved.rs`: the stderr sink receives the line `cleanup failed: db would not stop`.
  - `json.rs`: the `run_finished` object has `"cleanup_errors": ["db would not stop"]`; with no error the
    field is present and empty.

- [ ] **Step 6: Run them, expect failure.** Run: `cargo test -p alba render` Expected: FAIL on the three new
      tests.

- [ ] **Step 7: Implement** the rendering, and add `cleanup_errors` to the `run_finished` event in the
      README's "The JSON stream" section, described as: errors met while releasing what executors held for
      the run; non-empty makes the exit code 1.

- [ ] **Step 8: Run** `alba run check`. Expected: `all checks passed`.

- [ ] **Step 9: Commit**

  ```bash
  git add crates README.md
  git commit -m "✨ feat(executors): let an executor release state after a run"
  ```

---

### Task 2: Shutdown when a watch or TUI session ends

**Files:**

- Modify: `crates/alba-engine/src/session.rs`, `crates/alba-engine/src/watch/mod.rs`,
  `crates/alba-engine/src/lib.rs`
- Modify: `crates/alba/src/commands/run.rs`
- Test: `crates/alba-engine/tests/watch.rs`, inline tests in `run.rs`

**Interfaces:**

- Consumes: `Executors::shutdown`, `FakeEvent::ShutDown`, `FakeExecutor::fail_shutdown` (Task 1).
- Produces:
  - `pub struct SessionEnd { pub exit: WatchExit, pub cleanup_errors: Vec<String> }`, exported from
    `alba-engine`.
  - `session(...)` and `watch(...)` return `SessionEnd`. `executors.shutdown()` is called exactly once, after
    the last run has wound down, on every way out of the session (interrupt, `SessionCommand::Shutdown`,
    watcher closed). Keep the existing body as an inner function returning `WatchExit` and wrap it, so no
    return point can skip the call.
  - In `run.rs`: the watch command prints `cleanup failed: <message>` on stderr for each error and exits 1
    when there is any, otherwise its existing code. The TUI path reports the same lines once the interface
    has left the screen, through the existing `session_failure` mechanism, and exits 1.

- [ ] **Step 1: Write the failing tests** in `crates/alba-engine/tests/watch.rs`, with its `start` helper:

  - `a_session_shuts_its_executors_down_once_when_it_ends`: let the first run finish, trigger a second run
    with a file batch, let it finish, cancel the session. The fake's events contain exactly one `ShutDown`
    and it is the last event; the `SessionEnd` has `exit == WatchExit::Interrupted` and no cleanup error.
  - `a_session_that_loses_its_watcher_still_shuts_down`: close the watcher; one `ShutDown`,
    `exit == WatchExit::WatcherClosed`.
  - `a_session_reports_what_shutdown_could_not_release`: `fail_shutdown("db would not stop")`;
    `cleanup_errors == ["db would not stop"]`.

- [ ] **Step 2: Run them, expect failure.** Run: `cargo test -p alba-engine --test watch shut` Expected: does
      not compile (`SessionEnd` unknown).

- [ ] **Step 3: Implement** `SessionEnd` and the wrapping in `session.rs`; update `watch`.

- [ ] **Step 4: Run** `cargo test -p alba-engine`. Expected: PASS.

- [ ] **Step 5: Write the failing CLI tests**, inline in `run.rs` next to the existing `session_failure`
      tests: an interrupted session with `cleanup_errors: ["db would not stop"]` yields a failure message
      containing `cleanup failed: db would not stop`; an interrupted session with none still yields `None`.

- [ ] **Step 6: Run them, expect failure.** Run: `cargo test -p alba session_failure`

- [ ] **Step 7: Implement** the two call sites in `run.rs`.

- [ ] **Step 8: Run** `alba run check`. Expected: `all checks passed`.

- [ ] **Step 9: Commit**

  ```bash
  git add crates
  git commit -m "✨ feat(engine): shut executors down when a session ends"
  ```

---

### Task 3: Share the container plumbing between executors

A refactor with no behaviour change. Its proof is the docker executor's existing tests.

**Files:**

- Create: `crates/alba-executors/src/container.rs`
- Modify: `crates/alba-executors/src/docker.rs`, `crates/alba-executors/src/lib.rs`

**Interfaces:**

- Produces, all `pub(crate)` in `container.rs`, moved out of `docker.rs` with their doc comments:
  - `const STOP_GRACE: Duration` (5 s), `const DRAIN_PERIOD: Duration` (2 s)
  - `fn container_name(beam: &str) -> String`
  - `fn exec_args(container: &str, cwd: &str, env: &[(String, String)], command: &str) -> Vec<String>`
  - `async fn exec(container: &str, cwd: &str, cmd: &CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError>`:
    the whole body of `DockerSession::execute` after the working directory is computed, cancellation ladder
    and output draining included.
  - `async fn remove(container: &str) -> Result<(), ExecError>`: the body of `DockerSession::close`.
  - `async fn force_remove(container: &str)`: the body of `DockerSession::kill`.
  - `async fn run_quiet(args: &[&str])`
  - `fn spawn_error(error: std::io::Error, requirement: &str) -> ExecError`, message
    `` cannot run `docker`: {error} — {requirement} ``. The docker executor passes
    `the docker executor requires Docker on the PATH`, which keeps its message byte-identical.

- [ ] **Step 1: Move** the items above; `DockerSession` delegates to them. The unit tests
      `container_names_are_sanitized_and_unique` and `exec_args_carry_workdir_and_env` move to
      `container.rs` unchanged.

- [ ] **Step 2: Run the fast tests.** Run: `cargo test -p alba-executors` Expected: PASS, same test count as
      before the move.

- [ ] **Step 3: Run the docker daemon tests.** Run:
      `cargo test -p alba-executors --test docker -- --ignored` and
      `cargo test -p alba --test cli_docker -- --ignored` Expected: PASS, with no test edited.

- [ ] **Step 4: Run** `alba run check`, then commit.

  ```bash
  git add crates/alba-executors
  git commit -m "♻️ refactor(executors): share the container plumbing"
  ```

---

### Task 4: Read a Compose service and map a directory into it

Pure code: no process is spawned in this task.

**Files:**

- Create: `crates/alba-executors/src/compose/mod.rs` (declares the submodule only, for now),
  `crates/alba-executors/src/compose/service.rs`
- Modify: `crates/alba-executors/src/lib.rs` (`mod compose;`)

**Interfaces:**

- Produces, `pub(super)` in `service.rs`:

  ```rust
  pub(super) struct Bind { pub source: PathBuf, pub target: String }

  pub(super) struct ServiceDescription {
      pub project: String,                  // `name` in the config
      pub service: String,
      pub binds: Vec<Bind>,                 // `type == "bind"` only, in declaration order
      pub has_build: bool,
      pub direct_dependencies: Vec<String>, // keys of the service's `depends_on`, sorted
      pub dependencies: Vec<String>,        // transitive closure, sorted, without the service itself
  }

  impl ServiceDescription {
      pub fn parse(config_json: &str, service: &str) -> Result<Self, ExecError>;
      pub fn container_cwd(&self, host_cwd: &Path) -> Result<String, ExecError>;
  }
  ```

- `container_cwd` canonicalizes `host_cwd` (falling back to the path as given when it cannot be resolved),
  picks the bind whose `source` is a prefix of it with the most components, and returns the bind's `target`
  followed by the remaining components joined with `/`. A target of `/` must not produce a doubled slash.
- Its error, when no bind covers the directory:
  `` no bind mount of service `php` covers `/host/elsewhere` (bind mounts: /repo -> /app, /cache -> /c) ``, or
  `(the service has no bind mount)` when the list is empty.
- `parse` errors: invalid JSON, or `services.<service>` missing, as
  `` cannot read the description of service `php` from `docker compose config`: <cause> ``.

- [ ] **Step 1: Write the failing tests**, inline in `service.rs`. The fixture is the config of a service
      `tool` with a bind `/repo -> /app`, a named volume on `/app/node_modules`, `depends_on: {db: ...}`,
      next to `db` with `depends_on: {cache: ...}` and `cache` with none. Path-mapping tests use paths that
      do not exist, so canonicalization falls back and the tests run on every platform; mark the symlink
      test `#[cfg(unix)]`.

  - `parse_reads_the_project_the_binds_and_the_build_flag`: `project == "albaspike"`, one bind
    (`/repo`, `/app`), `has_build == false`; with a `build` key present, `true`.
  - `parse_separates_direct_dependencies_from_their_closure`: `direct_dependencies == ["db"]`,
    `dependencies == ["cache", "db"]`.
  - `parse_survives_a_dependency_cycle_free_diamond`: `tool -> a, b`, `a -> c`, `b -> c`:
    `dependencies == ["a", "b", "c"]`.
  - `parse_rejects_a_config_without_the_service`: the error names the service.
  - `a_directory_under_a_bind_maps_under_its_target`: `/repo/modules/facturation` gives
    `/app/modules/facturation`; `/repo` gives `/app`.
  - `the_longest_covering_bind_wins`: binds `/repo -> /app` and `/repo/apps/site -> /site`;
    `/repo/apps/site/src` gives `/site/src`.
  - `a_bind_is_a_prefix_by_component_not_by_text`: bind `/repo -> /app`; `/repository/x` is not covered.
  - `a_directory_no_bind_covers_is_an_error_listing_the_binds`: the message is the one specified above.
  - `a_service_with_only_named_volumes_covers_nothing`: the message ends with
    `(the service has no bind mount)`.
  - `a_bind_on_the_container_root_does_not_double_the_slash`: bind `/repo -> /`; `/repo/a` gives `/a`.
  - `a_directory_reached_through_a_symlink_still_maps` (`#[cfg(unix)]`): in a temp directory, `real/sub` and
    a symlink `link -> real`; a bind whose source is the canonical `real`; `link/sub` maps to `<target>/sub`.

- [ ] **Step 2: Run them, expect failure.** Run: `cargo test -p alba-executors compose::service` Expected:
      does not compile.

- [ ] **Step 3: Implement** `ServiceDescription`.

- [ ] **Step 4: Run them, expect success.** Same command. Expected: 11 passed.

- [ ] **Step 5: Run** `alba run check`, then commit. `#[allow(dead_code)]` is not acceptable: if the compiler
      warns that the module is unused, fold this task's commit into Task 5's instead of silencing it.

  ```bash
  git add crates/alba-executors
  git commit -m "✨ feat(executors): map a directory into a compose service"
  ```

---

### Task 5: Run a beam in a Compose service

**Files:**

- Modify: `crates/alba-executors/src/compose/mod.rs`, `crates/alba-executors/src/lib.rs`
  (`pub use compose::ComposeExecutor;`)
- Test: `crates/alba-executors/tests/compose.rs` (new)

**Interfaces:**

- Consumes: `container::{container_name, exec, remove, force_remove, run_quiet, spawn_error, STOP_GRACE}`
  (Task 3); `ServiceDescription` (Task 4).
- Produces:
  - `pub struct ComposeExecutor` with `pub fn new() -> Self` and `Default`.
  - Options, deserialized from `BeamContext::options`: `{"service": String, "file": Option<String>}`. A
    missing or empty `service` is `invalid compose options: ...`.
  - `open`, in order, each step raced against `beam.cancel`:
    1. `docker compose config --format json <service>`, stdout captured, stderr relayed to `beam.output`;
       then `ServiceDescription::parse`.
    2. When `has_build`: `docker compose build <service>`, stderr relayed.
    3. (Dependencies: Task 6 inserts them here.)
    4. `docker compose run --rm -T --no-deps --name <name> --label alba.beam=<beam> <service> sh`, stdin
       piped and kept, stderr relayed, stdout relayed line by line.
    5. Write `echo alba-ready-<name>\n` to the pipe and wait for that exact line on stdout; it is swallowed,
       not relayed. If the `run` process exits first, fail with
       `` docker compose run exited with code {code} for service `{service}` `` and remove `<name>` (bounded
       by `STOP_GRACE`).
  - A non-zero `config` or `build` fails with `` docker compose {subcommand} exited with code {code} for
    service `{service}` ``; Compose's own message is already on the beam's output.
  - Cancellation during `open` kills the in-flight process, removes `<name>` when the `run` had been
    spawned, and fails with `compose session start cancelled`.
  - `execute`: `description.container_cwd(&cmd.cwd)?`, then `container::exec`.
  - `close`: drop the stdin handle; wait for the `run` process up to `STOP_GRACE`, killing it past that;
    then `container::remove(name)`.
  - `kill`: `container::force_remove(name)`.
  - The spawn failure message is `container::spawn_error(error, "the compose executor requires Docker with
    the Compose plugin on the PATH")`.

- [ ] **Step 1: Write the test support and the first failing test** in `tests/compose.rs`. Copy the helper
      shapes of `tests/docker.rs` (`beam_context`, `exec_context`, `spec`, `drain_text`, `cancel_after`).
      Add:

  ```rust
  /// A throwaway Compose project in a temp directory. `yaml` is the file's content without its `name:`
  /// line; the project name is `alba-it-<suffix>-<pid>`. Dropping it runs
  /// `docker compose --profile "*" down -v` on that project and removes every container labelled
  /// `alba.beam=<name>`.
  struct ComposeProject { dir: tempfile::TempDir, name: String }
  impl ComposeProject {
      fn new(suffix: &str, yaml: &str) -> Self;
      fn path(&self) -> &Path;
      fn compose(&self, args: &[&str]) -> std::process::Output; // `docker compose <args>` in the project
      fn container_id(&self, service: &str) -> Option<String>;  // from `ps -a -q <service>`
  }
  ```

  First test, `a_command_runs_in_the_service_in_the_translated_directory`: service `tool`
  (`image: alpine:3`, `working_dir: /app`, `volumes: [".:/app"]`); the beam's directory is
  `<project>/modules/billing`; the command `pwd && echo "$GREETING"` with `env [("GREETING", "hej")]`
  outputs `/app/modules/billing` then `hej`, exit code 0.

- [ ] **Step 2: Run it, expect failure.** Run:
      `cargo test -p alba-executors --test compose -- --ignored` Expected: does not compile
      (`ComposeExecutor` unknown).

- [ ] **Step 3: Implement** `ComposeExecutor` and its session.

- [ ] **Step 4: Run it, expect success.** Same command. Expected: 1 passed.

- [ ] **Step 5: Add the remaining tests, one red-green cycle each**, same file, same `#[ignore]`:

  - `state_left_by_one_command_is_visible_to_the_next`: `touch /tmp/marker`, then `ls /tmp/marker` exits 0.
  - `the_exit_code_of_a_failing_command_is_reported`: `exit 7` gives `exit_code == 7`.
  - `a_service_behind_a_profile_is_reachable`: `profiles: [tools]` on the service.
  - `a_declared_file_is_used_instead_of_the_lookup`: the only Compose file is `ops/tools.yaml`, the beam's
    directory is the project root, options carry `"file": "ops/tools.yaml"`; a command runs.
  - `two_sessions_on_one_service_run_at_the_same_time`: open two sessions, both stay open, `docker ps`
    filtered on each beam's label shows one running container each.
  - `the_service_command_and_ports_are_not_used`: service with `command: sleep 3600` and
    `ports: ["38080:80"]`; the session's container has no published port (`docker port <name>` prints
    nothing) and `echo ok` runs.
  - `a_running_service_of_the_project_is_untouched`: `up -d web` first; after a session on `tool` opens and
    closes, `container_id("web")` is unchanged and the container is still running.
  - `the_image_is_built_when_the_service_declares_a_build`: a `build:` context with a Dockerfile writing
    `/built`; `test -f /built` exits 0, and nothing but the command's own output reached the beam.
  - `a_failing_build_puts_its_log_on_the_beams_output`: a Dockerfile with `RUN echo boom-marker && false`;
    `open` fails and the output contains `boom-marker`.
  - `an_unknown_service_fails_with_composes_message`: the output contains `no such service: nope`.
  - `a_directory_outside_every_bind_fails_the_command`: `execute` with a `cwd` outside the project fails with
    a message containing `no bind mount of service`.
  - `a_service_without_a_shell_fails_and_leaves_no_container`: `image: hello-world`; `open` fails with a
    message naming the service, and `docker ps -a` filtered on the beam's label is empty.
  - `closing_removes_the_container`: after `close`, `docker ps -a` filtered on the label is empty.
  - `cancelling_a_command_removes_the_container`: `sleep 60`, cancel after 500 ms; `execute` returns within
    `STOP_GRACE` plus a margin, and the container is gone.
  - `a_container_whose_stdin_closes_is_removed`: open a session, then drop it **without** calling `close`
    or `kill`; within 10 s the container is gone. This is the stand-in for a killed Alba: only the closing
    pipe removes it, so the `run` child must not be spawned with `kill_on_drop`.
  - `cancelling_the_open_leaves_no_container`: cancel 100 ms into `open`; it fails with
    `compose session start cancelled` and no container labelled for the beam remains.

- [ ] **Step 6: Run the whole file.** Run: `cargo test -p alba-executors --test compose -- --ignored`
      Expected: 17 passed. Then `docker ps -a --filter label=alba.beam` shows none of this file's beams.

- [ ] **Step 7: Run** `alba run check`, then commit.

  ```bash
  git add crates/alba-executors
  git commit -m "✨ feat(executors): run a beam in a compose service"
  ```

---

### Task 6: Start dependencies, and put them back at shutdown

**Files:**

- Create: `crates/alba-executors/src/compose/started.rs`
- Modify: `crates/alba-executors/src/compose/mod.rs`
- Test: inline in `started.rs`; `crates/alba-executors/tests/compose.rs`

**Interfaces:**

- Consumes: `Executor::shutdown` (Task 1); `ServiceDescription::{project, direct_dependencies, dependencies}`
  (Task 4); the session of Task 5.
- Produces, `pub(super)` in `started.rs`:

  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(super) enum Before { Absent, Stopped }

  /// What `docker compose ps -a --format json` printed, as service -> is it running.
  pub(super) fn running_by_service(ps_output: &str) -> Result<HashMap<String, bool>, ExecError>;

  /// The state to restore for each dependency that is not running. Running ones are absent from the result.
  pub(super) fn to_restore(dependencies: &[String], states: &HashMap<String, bool>) -> Vec<(String, Before)>;

  /// The dependencies Alba started in one Compose project, and how to address that project again.
  pub(super) struct ProjectRecord {
      pub dir: PathBuf,
      pub file: Option<PathBuf>,
      pub started: BTreeMap<String, Before>,
  }
  impl ProjectRecord {
      /// Adds dependencies not yet recorded. One already recorded keeps its first state.
      pub fn record(&mut self, restore: Vec<(String, Before)>);
      /// The services to `rm -s -f` and the services to `stop`, each sorted.
      pub fn release_plan(&self) -> (Vec<String>, Vec<String>);
  }
  ```

- `ComposeExecutor` holds `Mutex<HashMap<String, Arc<tokio::sync::Mutex<ProjectRecord>>>>` keyed by
  `ServiceDescription::project`.
- `open`, step 3, when `dependencies` is not empty, under that project's lock:
  1. `docker compose ps -a --format json`, parsed with `running_by_service`.
  2. `record(to_restore(&description.dependencies, &states))`, **before** the `up`, so a partial start is
     still put back.
  3. `docker compose up -d --no-recreate --wait <direct_dependencies...>`, stdout discarded, stderr relayed.
     A non-zero exit fails `open` with `` docker compose up exited with code {code} for the dependencies of
     service `{service}` ``.
- `ComposeExecutor::shutdown`: for every project, `docker compose rm -s -f <absent...>` then
  `docker compose stop <stopped...>` (each skipped when its list is empty), stdout discarded. Every project
  is attempted; the record is emptied whatever the outcome. Failures are joined with `; ` into one
  `ExecError`, each as `` could not release the dependencies of compose project `{project}`: {stderr} ``.
- `close` and `kill` do not touch dependencies.

- [ ] **Step 1: Write the failing unit tests**, inline in `started.rs`:

  - `running_by_service_reads_one_object_per_line`: two lines (`db` running, `cache` exited) give
    `{db: true, cache: false}`; an empty output gives an empty map.
  - `a_running_dependency_is_not_alba_s_to_restore`: `to_restore(["cache", "db"], {db: true})` gives
    `[("cache", Before::Absent)]`.
  - `a_stopped_dependency_is_restored_to_stopped`: `{cache: false}` gives `[("cache", Before::Stopped)]`.
  - `a_dependency_recorded_twice_keeps_its_first_state`: record `db` as `Absent`, then again as `Stopped`
    (what a second beam sees once the first has been cancelled mid-start); `release_plan` still removes it.
  - `the_release_plan_separates_removals_from_stops`: `{a: Absent, b: Stopped, c: Absent}` gives
    `(["a", "c"], ["b"])`.

- [ ] **Step 2: Run them, expect failure.** Run: `cargo test -p alba-executors compose::started`

- [ ] **Step 3: Implement** `started.rs`. Run again. Expected: 5 passed.

- [ ] **Step 4: Add the daemon tests, one red-green cycle each**, in `tests/compose.rs`. The fixture is
      `tool` with `depends_on: [db]`, `db` being `alpine:3` running `sleep 3600` with
      `stop_grace_period: 1s`:

  - `an_absent_dependency_is_started_kept_after_close_and_removed_at_shutdown`: after `open`, `db` runs;
    after `close`, it still runs; after `executor.shutdown()`, `container_id("db")` is `None`.
  - `a_stopped_dependency_is_started_then_stopped_again`: `up -d db` then `stop db` first; after `shutdown`
    the container still exists, same id, not running.
  - `a_running_dependency_is_left_alone`: `up -d db` first; after `open`, `close`, `shutdown`, same id, still
    running.
  - `a_running_dependency_whose_definition_changed_is_not_recreated`: `up -d db`, rewrite the Compose file to
    add an environment variable to `db`, then `open`; same id.
  - `a_second_session_reuses_the_dependency_the_first_started`: two sessions opened and closed in turn; the
    id of `db` is the same during both; one `shutdown` removes it.
  - `sessions_opening_together_start_a_dependency_once`: `tokio::join!` two `open` calls; both succeed, one
    `db` container, removed at `shutdown`.
  - `a_healthcheck_is_waited_for`: `db` becomes healthy after 2 s (`healthcheck` testing a file its command
    creates after a `sleep 2`), `condition: service_healthy`; the first command of the session finds
    `docker inspect` reporting `healthy`.
  - `a_one_shot_dependency_runs_to_completion`: `migrate` running `true`,
    `condition: service_completed_successfully`; `open` succeeds and `shutdown` removes its container.
  - `a_dependency_that_cannot_start_fails_the_beam_and_the_rest_is_put_back`: `tool` depends on `db` and on
    `ghost` (`image: alba-it-no-such-image:0`, `pull_policy: never`); `open` fails with a message naming
    the service; after `shutdown`, neither `db` nor `ghost` has a container.
  - `cancelling_a_command_keeps_the_dependencies_until_shutdown`: cancel a `sleep 60`; `db` still runs after
    `close`; `shutdown` removes it.
  - `shutdown_never_removes_a_volume`: `db` mounts a named volume `data`; after `shutdown`,
    `docker volume ls` still lists `<project>_data`.
  - `shutdown_with_nothing_started_succeeds`: `ComposeExecutor::new().shutdown()` returns `Ok(())`. This one
    needs no daemon: write it without `#[ignore]`.

- [ ] **Step 5: Run the whole file.** Run: `cargo test -p alba-executors --test compose -- --ignored`
      Expected: 28 passed, and `cargo test -p alba-executors --test compose` 1 passed.

- [ ] **Step 6: Run** `alba run check`, then commit.

  ```bash
  git add crates/alba-executors
  git commit -m "✨ feat(executors): manage a compose service's dependencies"
  ```

---

### Task 7: The `compose` block, from the Beamfile to the executor

**Files:**

- Modify: `crates/alba-core/src/model.rs`, `crates/alba-core/src/eval.rs`
- Modify: `crates/alba-engine/src/scheduler.rs`
- Modify: `crates/alba/src/commands/run.rs`
- Test: inline in `eval.rs` and `scheduler.rs`; `crates/alba-engine/tests/scheduler.rs`;
  `crates/alba/tests/cli_check_list.rs`; `crates/alba/tests/cli_compose.rs` (new)

**Interfaces:**

- Consumes: `ComposeExecutor::new()` (Task 5).
- Produces:
  - `ExecutorKind::Compose { service: String, file: Option<String> }`.
  - `build_compose(decl, options)` in `eval.rs`, dispatched from the `"compose"` arm. Messages:
    - `` executor `compose` requires a `service` option ``
    - `` executor `compose` option `service` must be a non-empty string ``
    - `` executor `compose` option `service` must not start with `-` ``
    - `` executor `compose` option `file` must be a non-empty string ``
    - `` executor `compose` does not take an option `{other}` ``, with the `did you mean` help over
      `["service", "file"]`.
  - `executor_label`: `compose`, then `push_field(service)`, then `push_field(file or "")`.
  - `executor_options`: `{"service": service, "file": file}`.
  - `Executors::compose: Arc<dyn Executor>`; `for_beam` dispatches `ExecutorKind::Compose` to it;
    `Executors::uniform` and `Executors::shutdown` include it.
  - `run.rs`: `compose: Arc::new(ComposeExecutor::new())`.

- [ ] **Step 1: Write the failing evaluation tests**, inline in `eval.rs`, next to the docker ones:

  - `executor_compose_carries_its_service`:
    `beam t { executor compose { service "php" } run "x" }` gives
    `ExecutorKind::Compose { service: "php".into(), file: None }`.
  - `executor_compose_carries_its_file`: with `file "../../compose.yaml"`.
  - `compose_options_interpolate`: `let svc = "php"` and `service "{svc}"` give `"php"`.
  - `compose_requires_a_service`: `executor compose { }` and `executor compose { file "c.yaml" }`.
  - `compose_service_must_be_a_non_empty_string`: `service ""` and `service ["php"]`.
  - `compose_service_must_not_look_like_a_flag`: `service "--help"`.
  - `compose_file_must_be_a_non_empty_string`: `file ""`.
  - `compose_rejects_an_unknown_option_and_suggests_the_nearest`: `servce "php"` gives the "does not take an
    option" message with help `` did you mean `service`? ``; `user "1000"` gives it with no help.

- [ ] **Step 2: Run them, expect failure.** Run: `cargo test -p alba-core compose`

- [ ] **Step 3: Implement** the model variant and `build_compose`; make the workspace compile by completing
      every match on `ExecutorKind` (`executor_label`, `executor_options`, `for_beam`) and adding the
      `compose` slot to `Executors` and to `run.rs`.

- [ ] **Step 4: Run** `cargo test -p alba-core compose`. Expected: 8 passed.

- [ ] **Step 5: Write the failing engine tests.**

  - In `crates/alba-engine/tests/scheduler.rs`, `a_compose_beam_dispatches_to_the_compose_slot_with_its_options`,
    modelled on `a_docker_beam_dispatches_to_the_docker_slot_with_its_options`: only the fake in the
    `compose` slot sees `Opened { beam, options: json!({"service": "php", "file": null}) }`.
  - Inline in `scheduler.rs`, next to the docker label tests,
    `a_compose_label_changes_with_its_service_and_its_file`: the labels of `{php, None}`, `{node, None}` and
    `{php, Some("c.yaml")}` are pairwise different, and `{php, None}` differs from
    `Docker { image: Some("php"), .. }`.

- [ ] **Step 6: Run** `cargo test -p alba-engine compose`. If Step 3 already made them pass, break each
      assertion once to see it fail for the right reason, then restore it.

- [ ] **Step 7: Write the CLI tests.**

  - In `cli_check_list.rs` (no daemon), `check_accepts_a_compose_beam_without_docker`: a Beamfile with
    `executor compose { service "php" }` passes `alba check` with `PATH` emptied of `docker`.
  - In the new `cli_compose.rs`, `#[ignore = "requires a running docker daemon"]`, with the helper shapes of
    `cli_docker.rs` and a throwaway Compose project as in Task 5:
    - `a_module_two_directories_down_runs_in_the_root_service`: `compose.yaml` at the root with a service
      binding `.` on `/app`; `modules/billing/Beamfile` with a beam running `pwd`; `alba run` from
      `modules/billing` prints `/app/modules/billing`; `alba run billing:<beam>` from the root, through an
      importing root Beamfile, prints the same.
    - `beams_on_one_service_run_in_parallel`: two beams, each `sleep 2`, needed by a third; the run takes
      under 4 s.
    - `a_run_leaves_no_container_and_no_dependency_it_started`: a service depending on `db`; after
      `alba run`, no container carries the `alba.beam` label and `db` has no container.
    - `a_dependency_already_running_survives_the_run`: `docker compose up -d db` first; same container id
      after `alba run`.

- [ ] **Step 8: Run them.** Run: `cargo test -p alba --test cli_check_list compose` and
      `cargo test -p alba --test cli_compose -- --ignored` Expected: 1 passed, 4 passed.

- [ ] **Step 9: Run** `alba run check`, then commit. The body states the reserved name, since the changelog is
      generated from this message.

  ```bash
  git add crates
  git commit -m "✨ feat(executors): add the compose executor" -m "A beam declares \`executor compose { service \"php\" }\` to run in a
  one-off container of a Compose service.

  \`compose\` is now a built-in executor name: a plugin binary named
  \`alba-executor-compose\` is no longer reachable."
  ```

---

### Task 8: Documentation and continuous integration

**Files:**

- Modify: `README.md`, `alba-agent-plugin/skills/using-alba/SKILL.md`,
  `alba-agent-plugin/skills/using-alba/references/beamfile.md`, `.github/workflows/ci.yml`
- Modify: `.claude/superpowers/specs/2026-11-10-alba-compose-executor-design.md` (status, changelog line)

- [ ] **Step 1: README.** In "Executors", add `executor compose { service "..." }` to the list and to the
      sentence on reserved names. Add a "Compose executor" section right after "Docker executor", covering,
      in this order, each with what the spec says:

  1. the block and its two options, with the reference project's shape as the example (a root
     `compose.yaml`, a module Beamfile two directories down);
  2. how the Compose file is found, and `file`;
  3. the working directory rule and its error;
  4. what Alba passes and does not pass to Compose (no user, no ports, the service's command replaced);
  5. the image build and the `inputs` a beam should declare (`compose.yaml`, Dockerfiles), since the cache
     only sees the block's text;
  6. dependencies: started when absent, left alone when running, put back when the command ends, and the
     five accepted limits, verbatim from the spec's list;
  7. prerequisites (`docker` with the Compose plugin, `sh` in the image, an entrypoint that executes its
     arguments);
  8. manual cleanup after a crash: the existing `docker rm -f $(docker ps -aq --filter label=alba.beam)`,
     plus `docker compose stop <service>` for a dependency left up.

  In "Docker executor", add one sentence pointing projects that already have a Compose file to the new
  section. In the `Executor` paragraph of "Plugins", add `compose` to the names a plugin cannot take.

- [ ] **Step 2: Agent plugin.** In `SKILL.md`, extend the executors sentence of the mental model with
      `executor compose { service "..." }` runs in a service of the project's Compose file. In
      `references/beamfile.md`, document the block next to the docker one: both options, the working
      directory rule, the dependency lifecycle in two sentences.

- [ ] **Step 3: CI.** Rename the step `Test (docker executor)` to `Test (docker and compose executors)` and
      update the comment above it. The command already runs every ignored test.

- [ ] **Step 4: Spec.** Set `Status: approved`. Replace the `CHANGELOG.md` bullet of its "Documentation"
      section with: the changelog entry comes from the feature commit, whose body states the reserved name.

- [ ] **Step 5: Verify.** Run: `alba run check` Expected: `all checks passed`. Run:
      `cargo test --workspace --all-features -- --ignored` Expected: PASS (docker, compose, and the other
      ignored suites).

- [ ] **Step 6: Commit**

  ```bash
  git add README.md alba-agent-plugin .github
  git commit -m "📝 docs(executors): document the compose executor"
  git add .claude/superpowers/specs
  git commit -m "📝 docs(executors): mark the compose design approved"
  ```

---

## After the last task

Not part of this repository's change, and not to be done by the implementer: migrating the reference
project's Beamfiles is the acceptance run of the spec's success criteria, and it is done by hand against that
project once this branch is installed (`alba run install`).
