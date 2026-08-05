# Alba Docker Executor and Plugin Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the [docker/plugins spec](../specs/2026-08-06-alba-docker-plugins-design.md): rework the `Executor` contract into per-beam sessions, run docker beams in real containers via the `docker` CLI, and define protocol v1 for external executor plugins (PATH discovery, JSON Lines over stdin/stdout) with an example plugin and an `alba plugin check` conformance subcommand.

**Architecture:** `alba-executors` replaces the single-shot `Executor::execute` with `Executor::open(BeamContext) -> Box<dyn ExecSession>` (execute per command, then `close`, guaranteed by the engine). Docker and plugin executors are new modules in `alba-executors`; executor options travel as `serde_json::Value` so the crate stays free of `alba-core`. The engine serializes `ExecutorKind` into that JSON, resolves plugin binaries at plan time, and folds executor configuration into the cache key. The CLI wires `DockerExecutor` into its `Executors` and gains `alba plugin check`.

**Tech Stack:** Rust (edition 2024, cargo workspace), tokio, async-trait, serde/serde_json, which 7 (engine only), assert_cmd (e2e), insta (syntax snapshots).

## Global Constraints

- Dependency direction stays strictly downward: `cli → tui → engine → core → syntax`. `alba-executors` must depend on neither `alba-core` nor `alba-syntax` (its own crate doc comment states this).
- TDD (red, green, refactor) for every task; run the failing test before implementing.
- Full gate before every commit: `cargo build --workspace --all-features`, `cargo fmt --check` (run `cargo fmt` first), `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`.
- Every commit leaves the whole workspace green; no task may end with another crate broken.
- Docker-dependent tests are `#[ignore = "requires a running docker daemon"]`; they run in CI's linux job only (Task 11) and locally via `cargo test -- --ignored`.
- All file content in English. Commit messages: gitmoji + Conventional Commits (e.g. `✨ feat(engine): ...`). No Claude attribution anywhere.
- Protocol constants from the spec: `protocol: 1`, 10-second handshake timeout, 5-second cancellation grace, plugin binary naming `alba-executor-<name>`.

## File Structure

```
crates/alba-executors/src/lib.rs        # trait rework: Executor::open, ExecSession, BeamContext
crates/alba-executors/src/embedded.rs   # stateless session migration
crates/alba-executors/src/shell.rs      # stateless session migration; stream_lines made pub(crate)
crates/alba-executors/src/fake.rs       # session-aware FakeExecutor with an event log
crates/alba-executors/src/docker.rs     # NEW: DockerExecutor + DockerSession
crates/alba-executors/src/protocol.rs   # NEW: wire message types (pub, reused by the CLI)
crates/alba-executors/src/plugin.rs     # NEW: PluginExecutor + PluginSession
crates/alba-executors/tests/support/fake_plugin.rs  # NEW: scripted fake plugin [[bin]]
crates/alba-executors/tests/plugin.rs   # NEW: plugin executor tests against the fake plugin
crates/alba-executors/tests/docker.rs   # NEW: ignored docker integration tests
crates/alba-syntax/src/ast.rs           # ExecutorOptionValue (string | bool | list)
crates/alba-syntax/src/parser.rs        # parse the three option value forms
crates/alba-core/src/model.rs           # ExecutorKind::{Docker extended, Plugin}, OptionValue
crates/alba-core/src/eval.rs            # build_executor rewrite; suggest made pub
crates/alba-engine/src/scheduler.rs     # session lifecycle, executor_options, plugin resolution
crates/alba-engine/src/cache/fingerprint.rs  # BeamFacts.executor: &'a str
crates/alba-cli/src/args.rs             # `alba plugin check` subcommand
crates/alba-cli/src/main.rs             # dispatch `plugin` before loading the Beamfile
crates/alba-cli/src/commands/run.rs     # Executors construction gains docker
crates/alba-cli/src/commands/plugin.rs  # NEW: the conformance checker
crates/alba-executor-example/           # NEW: workspace crate, the example plugin binary
crates/alba-cli/tests/cli_plugin.rs     # NEW: plugin e2e + plugin check e2e
docs/plugin-protocol.md                 # NEW: the published protocol specification
.github/workflows/ci.yml               # linux docker step
```

---

### Task 1: The session contract

Rework the `Executor` trait into `open → ExecSession` in `alba-executors`, migrate the three existing implementations, and teach the engine to open lazily, execute per command, and close in every case.

**Files:**
- Modify: `crates/alba-executors/Cargo.toml`
- Modify: `crates/alba-executors/src/lib.rs`
- Modify: `crates/alba-executors/src/embedded.rs`
- Modify: `crates/alba-executors/src/shell.rs`
- Modify: `crates/alba-executors/src/fake.rs`
- Modify: `crates/alba-engine/src/scheduler.rs`
- Test: `crates/alba-engine/tests/scheduler.rs`, existing `crates/alba-executors/tests/{embedded,shell}.rs`

**Interfaces:**
- Produces (all later tasks build on these exact signatures):

```rust
// alba-executors/src/lib.rs
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError>;
}

#[async_trait::async_trait]
pub trait ExecSession: Send {
    async fn execute(&mut self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError>;
    async fn close(self: Box<Self>) -> Result<(), ExecError>;
}

/// Everything a session needs to exist before its first command: the
/// beam's label (names containers and plugin processes), its directory,
/// the executor options as JSON (the engine serializes `ExecutorKind`
/// into this; `Null` for the shell executors), an output channel for
/// setup-time lines (an image pull), and the run's cancellation token.
pub struct BeamContext {
    pub beam: String,
    pub dir: std::path::PathBuf,
    pub options: serde_json::Value,
    pub output: tokio::sync::mpsc::UnboundedSender<OutputLine>,
    pub cancel: tokio_util::sync::CancellationToken,
}
```

- `FakeExecutor` (test-util) additionally produces:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum FakeEvent {
    Opened { beam: String, options: serde_json::Value },
    Executed { command: String },
    Closed { beam: String },
}
impl FakeExecutor {
    pub fn events(&self) -> Vec<FakeEvent>;
    /// Makes `open` fail for any beam whose label contains the substring.
    pub fn fail_open(self, beam_substring: impl Into<String>, message: impl Into<String>) -> Self;
}
```

- [ ] **Step 1: Write the failing engine tests**

In `crates/alba-engine/tests/scheduler.rs`, following the file's existing helpers (read them first — it already builds projects via `alba_core::load_str` and runs with `Executors::uniform(Arc::new(fake))`), add:

```rust
#[tokio::test]
async fn a_beam_opens_one_session_runs_its_commands_in_it_and_closes_it() {
    // Beamfile: beam build { run ["echo a", "echo b"] }
    // Assert on fake.events(): exactly
    //   [Opened { beam: "build", .. }, Executed { command: "echo a" },
    //    Executed { command: "echo b" }, Closed { beam: "build" }]
}

#[tokio::test]
async fn the_session_closes_even_when_a_command_fails() {
    // Beamfile: beam build { run ["boom", "echo never"] }, fake scripted so
    // "boom" exits 1. Assert events end with Closed and contain no
    // Executed { command: "echo never" }.
}

#[tokio::test]
async fn the_session_closes_when_the_run_is_cancelled_mid_command() {
    // Fake behavior with a long delay on the only command; cancel the
    // caller's token after 50ms; assert the beam ends Cancelled and the
    // events contain Closed { beam: .. }.
}

#[tokio::test]
async fn an_open_failure_is_the_beams_failure_with_the_message_on_stderr() {
    // fake = FakeExecutor::new().fail_open("build", "no runtime here");
    // Assert the beam's status is Failed { exit_code: -1 }, the run's
    // summary counts it failed, and a BeamOutput stderr line carries
    // "no runtime here". No Executed/Closed events for that beam.
}
```

Write them with real assertions against the existing event-collection helpers in that test file.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p alba-engine --test scheduler -- session` — expected: compile error (`FakeExecutor` has no `events`, trait has no `open`). That is the red state for a contract change.

- [ ] **Step 3: Rework `alba-executors`**

`Cargo.toml`: add `serde_json.workspace = true` under `[dependencies]`.

`lib.rs`: replace the `Executor` trait with the two-trait contract and add `BeamContext` (exact code in Interfaces above). Update the crate doc comment: the session is the per-beam unit; `open`/`close` bracket a beam's commands; cleanup is the engine's responsibility.

`embedded.rs`: `EmbeddedShellExecutor::open` returns `Box::new(EmbeddedShellSession)`; move the old `execute` body onto:

```rust
struct EmbeddedShellSession;

#[async_trait::async_trait]
impl ExecSession for EmbeddedShellSession {
    async fn execute(&mut self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError> {
        // the previous EmbeddedShellExecutor::execute body, verbatim
    }
    async fn close(self: Box<Self>) -> Result<(), ExecError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl Executor for EmbeddedShellExecutor {
    async fn open(&self, _beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError> {
        Ok(Box::new(EmbeddedShellSession))
    }
}
```

`shell.rs`: same mechanical migration (`SystemShellSession`). While here, change `async fn stream_lines` from private to `pub(crate)` — the docker and plugin modules reuse it in Tasks 4 and 7.

`fake.rs`: restructure so a session can share the executor's interior state behind the existing `Arc<dyn Executor>` usage:

```rust
struct FakeState {
    behaviors: Mutex<Vec<(String, FakeBehavior)>>,
    calls: Mutex<Vec<CommandSpec>>,
    events: Mutex<Vec<FakeEvent>>,
    open_failures: Mutex<Vec<(String, String)>>,
    running: AtomicUsize,
    peak: AtomicUsize,
}

pub struct FakeExecutor {
    state: Arc<FakeState>,
}

struct FakeSession {
    state: Arc<FakeState>,
    beam: String,
}
```

`open` records `FakeEvent::Opened { beam, options }` (clone `beam.options`), checks `open_failures` for a substring match and returns `Err(ExecError { message })` on one; `FakeSession::execute` keeps the old `execute` body (calls/behaviors/running/peak) plus records `Executed`; `close` records `Closed`. Keep `calls()` and `running_peak()` working (delegate to `state`). Update the in-module tests to the new contract (open a session, execute through it).

- [ ] **Step 4: Migrate the engine's scheduler**

In `crates/alba-engine/src/scheduler.rs`:

Add next to `executor_label`:

```rust
/// What a session's `BeamContext.options` carries for this beam's kind.
/// The shell executors take no options; docker and plugin beams never get
/// here yet (rejected in `plan`), refined when their dispatch lands.
fn executor_options(kind: &ExecutorKind) -> serde_json::Value {
    match kind {
        ExecutorKind::Shell | ExecutorKind::SystemShell => serde_json::Value::Null,
        ExecutorKind::Docker { .. } => serde_json::Value::Null,
    }
}
```

Rework `run_commands` so the session brackets the loop and `close` always runs:

```rust
async fn run_commands(
    task: &BeamTask,
    plan: &RenderedBeam,
    output: &UnboundedSender<OutputLine>,
) -> BeamStatus {
    let context = BeamContext {
        beam: task.beam.id.0.clone(),
        dir: task.beam.dir.clone(),
        options: executor_options(&task.beam.executor),
        output: output.clone(),
        cancel: task.cancel.clone(),
    };
    let mut session = match task.executor.open(context).await {
        Ok(session) => session,
        Err(error) => {
            let _ = output.send(OutputLine {
                stream: Stream::Stderr,
                text: error.to_string(),
            });
            return if task.cancel.is_cancelled() {
                BeamStatus::Cancelled
            } else {
                failure_status(task)
            };
        }
    };

    let mut status = BeamStatus::Succeeded;
    for command in &plan.commands {
        let spec = CommandSpec {
            command: command.clone(),
            env: plan.env.clone(),
            cwd: plan.cwd.clone(),
        };
        let context = ExecContext {
            output: output.clone(),
            cancel: task.cancel.clone(),
        };

        let exit_code = match session.execute(spec, context).await {
            Ok(result) if result.exit_code == 0 => continue,
            Ok(result) => result.exit_code,
            Err(error) => {
                let _ = output.send(OutputLine {
                    stream: Stream::Stderr,
                    text: error.to_string(),
                });
                NO_EXIT_CODE
            }
        };

        status = if task.cancel.is_cancelled() {
            BeamStatus::Cancelled
        } else if task.beam.allow_failure {
            BeamStatus::FailedAllowed { exit_code }
        } else {
            BeamStatus::Failed { exit_code }
        };
        break;
    }

    // Guaranteed cleanup: success, failure, and cancellation all pass
    // here. A close failure is a notice, never a verdict change — the
    // commands' own outcome is already decided.
    if let Err(error) = session.close().await {
        let _ = output.send(OutputLine {
            stream: Stream::Stderr,
            text: format!("session close: {error}"),
        });
    }
    status
}
```

Import `BeamContext` from `alba_executors`. Nothing else in the engine changes: `Executors`, `for_beam`, and the cache path are untouched in this task, and the CLI compiles unmodified because `Arc<dyn Executor>` keeps its name.

- [ ] **Step 5: Run the new tests, then the whole workspace**

Run: `cargo test -p alba-engine --test scheduler` → the four new tests PASS. Then the full gate from Global Constraints — the executors' own test files (`tests/embedded.rs`, `tests/shell.rs`) will need the mechanical `open` + `execute` + `close` update; make it in the same task. Expected: everything green, no behavioral diff anywhere else (the e2e suite is the proof).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "♻️ refactor(executors): give every beam a session with open and close"
```

---

### Task 2: The DSL — extended docker block, plugin executors

Executor option values grow from strings to string | bool | list-of-strings; `docker` gains `volumes` and `workdir`; an unknown executor name becomes `ExecutorKind::Plugin` instead of an evaluation error. The engine temporarily rejects plugin beams at plan time (lifted in Task 8).

**Files:**
- Modify: `crates/alba-syntax/src/ast.rs`
- Modify: `crates/alba-syntax/src/parser.rs`
- Modify: `crates/alba-syntax/tests/parse.rs`
- Modify: `crates/alba-core/src/model.rs`
- Modify: `crates/alba-core/src/eval.rs`
- Modify: `crates/alba-engine/src/scheduler.rs` (compile fixes + temporary rejection)
- Modify: `crates/alba-cli/src/commands/check.rs` (doc comment only: plugin beams also skip the embedded-shell grammar check — the `!= ExecutorKind::Shell` filter already covers them)

**Interfaces:**
- Produces:

```rust
// alba-syntax/src/ast.rs
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutorOptionValue {
    Str(StringTemplate),
    Bool(bool),
    List(Vec<StringTemplate>),
}
pub struct ExecutorDecl {
    pub name: Spanned<String>,
    pub options: Vec<(Spanned<String>, ExecutorOptionValue)>,
}

// alba-core/src/model.rs
pub enum ExecutorKind {
    Shell,
    SystemShell,
    Docker { image: String, volumes: Vec<String>, workdir: Option<String> },
    Plugin { name: String, options: Vec<(String, OptionValue)> },
}
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue { Str(String), Bool(bool), List(Vec<String>) }
```

- Consumes: Task 1's engine layout (only match arms change).

- [ ] **Step 1: Write the failing parser tests**

In `crates/alba-syntax/tests/parse.rs` (follow its insta-snapshot style):

```rust
#[test]
fn executor_options_accept_bool_and_list_values() {
    // executor podman { image "quay.io/x" remote true volumes ["a:/b", "c:/d"] }
    // Snapshot the AST: three options with Str / Bool / List values.
}

#[test]
fn executor_option_list_reports_a_malformed_entry() {
    // executor docker { volumes ["a:/b", true] } — snapshot the error
    // (parse_template_list already rejects a non-string entry; the
    // snapshot pins the message and span).
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alba-syntax --test parse executor_option` → FAIL (no `ExecutorOptionValue`, parser rejects `true`).

- [ ] **Step 3: Implement the syntax side**

`ast.rs`: replace `options: Vec<NamedString>` on `ExecutorDecl` with the enum form above (keep `NamedString` for `env`).

`parser.rs` `parse_executor_decl` — dispatch on the value's first token:

```rust
while !self.check(&TokenKind::RBrace) {
    let opt_name = self.eat_ident()?;
    let opt_value = match self.peek().kind {
        TokenKind::KwTrue | TokenKind::KwFalse => {
            ExecutorOptionValue::Bool(self.parse_bool_value()?)
        }
        TokenKind::LBracket => ExecutorOptionValue::List(self.parse_template_list()?),
        _ => ExecutorOptionValue::Str(self.eat_template()?),
    };
    options.push((opt_name, opt_value));
}
```

Run `cargo test -p alba-syntax` and `cargo insta review` (accept the new snapshots). `alba-core` no longer compiles — that is the next step, in this same task.

- [ ] **Step 4: Write the failing core tests**

In `crates/alba-core/src/eval.rs` tests (same module as `executor_docker_carries_image`):

```rust
#[test]
fn executor_docker_carries_volumes_and_workdir() {
    let project = load_str(
        r#"beam d { executor docker { image "x" volumes ["h:/c"] workdir "/w" } run "x" }"#,
    ).unwrap();
    assert_eq!(project.beams[0].executor, ExecutorKind::Docker {
        image: "x".into(),
        volumes: vec!["h:/c".into()],
        workdir: Some("/w".into()),
    });
}

#[test]
fn docker_volumes_must_be_a_list() {
    // executor docker { image "x" volumes "h:/c" } →
    // "executor `docker` option `volumes` must be a list of strings"
}

#[test]
fn docker_volume_entries_must_name_a_container_path() {
    // volumes ["nocolon"] →
    // "invalid volume `nocolon`: expected `host:container`"
}

#[test]
fn docker_workdir_must_be_an_absolute_container_path() {
    // workdir "relative" →
    // "executor `docker` option `workdir` must be an absolute container path"
}

#[test]
fn docker_rejects_an_unknown_option_with_a_suggestion() {
    // executor docker { image "x" volume ["a:/b"] } →
    // "executor `docker` does not take an option `volume`" with help
    // "did you mean `volumes`?"
}

#[test]
fn an_unknown_executor_becomes_a_plugin_reference() {
    let project = load_str(
        r#"beam d { executor podman { image "x" remote true tags ["a", "b"] } run "x" }"#,
    ).unwrap();
    assert_eq!(project.beams[0].executor, ExecutorKind::Plugin {
        name: "podman".into(),
        options: vec![
            ("image".into(), OptionValue::Str("x".into())),
            ("remote".into(), OptionValue::Bool(true)),
            ("tags".into(), OptionValue::List(vec!["a".into(), "b".into()])),
        ],
    });
}

#[test]
fn shell_takes_no_options() {
    // executor shell { image "x" } → "executor `shell` takes no options"
    // (previously silently ignored; now consistent with system_shell)
}
```

Update the existing `unknown_executor_suggests_the_nearest_name` test: `dokcer` now loads successfully as `Plugin { name: "dokcer", .. }` — repurpose it to assert exactly that (the typo suggestion moves to plan time in Task 8).

- [ ] **Step 5: Run to verify they fail**

Run: `cargo test -p alba-core executor` → compile errors first (the AST changed), then assertion failures. Red confirmed.

- [ ] **Step 6: Implement the core side**

`model.rs`: the two-variant extension from Interfaces; update `ExecutorKind::Docker`'s doc comment ("the engine rejects this at run time" is no longer true once Task 5 lands — say the engine runs it in a container, and that `Plugin` names an external `alba-executor-<name>` binary resolved at plan time).

`eval.rs` `build_executor` rewrite:

```rust
fn build_executor(
    decl: Option<&ExecutorDecl>,
    lets: &Scope,
    params: &[String],
) -> Result<ExecutorKind, CoreError> {
    let Some(decl) = decl else {
        return Ok(ExecutorKind::Shell);
    };

    // Render every option at load time, whatever the kind, so a malformed
    // interpolation anywhere in the block is still caught.
    let mut options: Vec<(String, Span, OptionValue)> = Vec::new();
    for (key, value) in &decl.options {
        let rendered = match value {
            ExecutorOptionValue::Str(template) => OptionValue::Str(render_load_time_template(
                "executor options", template, lets, params,
            )?),
            ExecutorOptionValue::Bool(flag) => OptionValue::Bool(*flag),
            ExecutorOptionValue::List(templates) => OptionValue::List(
                templates
                    .iter()
                    .map(|t| render_load_time_template("executor options", t, lets, params))
                    .collect::<Result<_, _>>()?,
            ),
        };
        options.push((key.value.clone(), key.span, rendered));
    }

    match decl.name.value.as_str() {
        "shell" => no_options("shell", &options, decl).map(|()| ExecutorKind::Shell),
        "system_shell" => no_options("system_shell", &options, decl).map(|()| ExecutorKind::SystemShell),
        "docker" => build_docker(decl, options),
        other => Ok(ExecutorKind::Plugin {
            name: other.to_string(),
            options: options.into_iter().map(|(k, _, v)| (k, v)).collect(),
        }),
    }
}
```

with two helpers in the same file:

```rust
fn no_options(name: &str, options: &[(String, Span, OptionValue)], decl: &ExecutorDecl) -> Result<(), CoreError> {
    if options.is_empty() {
        Ok(())
    } else {
        Err(CoreError::new(
            format!("executor `{name}` takes no options"),
            decl.name.span,
        ))
    }
}

fn build_docker(
    decl: &ExecutorDecl,
    options: Vec<(String, Span, OptionValue)>,
) -> Result<ExecutorKind, CoreError> {
    let mut image = None;
    let mut volumes = Vec::new();
    let mut workdir = None;
    for (key, span, value) in options {
        match (key.as_str(), value) {
            ("image", OptionValue::Str(v)) => image = Some(v),
            ("image", _) => {
                return Err(CoreError::new(
                    "executor `docker` option `image` must be a string".to_string(), span,
                ));
            }
            ("volumes", OptionValue::List(entries)) => {
                for entry in &entries {
                    // `rsplit_once` so a windows host path (`C:\cache`)
                    // keeps its drive colon; only the last `:` splits.
                    let valid = matches!(entry.rsplit_once(':'), Some((host, path))
                        if !host.is_empty() && path.starts_with('/'));
                    if !valid {
                        return Err(CoreError::new(
                            format!("invalid volume `{entry}`: expected `host:container`"), span,
                        ));
                    }
                }
                volumes = entries;
            }
            ("volumes", _) => {
                return Err(CoreError::new(
                    "executor `docker` option `volumes` must be a list of strings".to_string(), span,
                ));
            }
            ("workdir", OptionValue::Str(v)) if v.starts_with('/') => workdir = Some(v),
            ("workdir", _) => {
                return Err(CoreError::new(
                    "executor `docker` option `workdir` must be an absolute container path".to_string(), span,
                ));
            }
            (other, _) => {
                let err = CoreError::new(
                    format!("executor `docker` does not take an option `{other}`"), span,
                );
                return Err(match suggest(other, ["image", "volumes", "workdir"].into_iter()) {
                    Some(c) => err.with_help(format!("did you mean `{c}`?")),
                    None => err,
                });
            }
        }
    }
    let image = image.ok_or_else(|| CoreError::new(
        "executor `docker` requires an `image` option".to_string(),
        decl.name.span,
    ))?;
    Ok(ExecutorKind::Docker { image, volumes, workdir })
}
```

Export `OptionValue` from `lib.rs` (`pub use model::{..., OptionValue}`).

- [ ] **Step 7: Fix the engine's exhaustive matches**

`scheduler.rs`: `for_beam` and `executor_label` gain a `Plugin` arm marked `unreachable!("plugin executors are rejected during validation, before scheduling")` (mirroring docker's); `executor_options` gains `ExecutorKind::Plugin { .. } => serde_json::Value::Null`; `plan` extends the rejection:

```rust
if beams.iter().any(|beam| matches!(beam.executor, ExecutorKind::Plugin { .. })) {
    return Err(EngineError::Unschedulable(
        "plugin executors are not yet supported".to_string(),
    ));
}
```

(Temporary — removed in Task 8. Docker's rejection stays until Task 5.)

- [ ] **Step 8: Full gate, then commit**

Run the full gate. Then:

```bash
git add -A
git commit -m "✨ feat(dsl): extend the docker block and admit plugin executor names"
```

---

### Task 3: Executor configuration in the cache key

The cache label becomes a `String` that folds in the executor's configuration, so changing a docker image or a plugin option invalidates the beam's cache entry.

**Files:**
- Modify: `crates/alba-engine/src/scheduler.rs`
- Modify: `crates/alba-engine/src/cache/fingerprint.rs`

**Interfaces:**
- Consumes: Task 2's `ExecutorKind`/`OptionValue`.
- Produces: `fn executor_label(kind: &ExecutorKind) -> String` (scheduler-private) and `BeamFacts { pub executor: &'a str, .. }`.

- [ ] **Step 1: Write the failing tests**

`executor_label` is private to `scheduler.rs`; test it in a `#[cfg(test)]` module inside that file (create one if absent):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_labels_are_unchanged() {
        assert_eq!(executor_label(&ExecutorKind::Shell), "embedded");
        assert_eq!(executor_label(&ExecutorKind::SystemShell), "system");
    }

    #[test]
    fn a_docker_label_changes_with_every_piece_of_its_configuration() {
        let base = ExecutorKind::Docker {
            image: "alpine:3".into(), volumes: vec![], workdir: None,
        };
        let other_image = ExecutorKind::Docker {
            image: "alpine:4".into(), volumes: vec![], workdir: None,
        };
        let with_volume = ExecutorKind::Docker {
            image: "alpine:3".into(), volumes: vec!["h:/c".into()], workdir: None,
        };
        let with_workdir = ExecutorKind::Docker {
            image: "alpine:3".into(), volumes: vec![], workdir: Some("/w".into()),
        };
        let labels: Vec<String> = [&base, &other_image, &with_volume, &with_workdir]
            .iter().map(|kind| executor_label(kind)).collect();
        let unique: std::collections::HashSet<&String> = labels.iter().collect();
        assert_eq!(unique.len(), 4, "every configuration must label distinctly: {labels:?}");
    }

    #[test]
    fn a_plugin_label_changes_with_its_options() {
        let a = ExecutorKind::Plugin {
            name: "podman".into(),
            options: vec![("image".into(), OptionValue::Str("x".into()))],
        };
        let b = ExecutorKind::Plugin {
            name: "podman".into(),
            options: vec![("image".into(), OptionValue::Str("y".into()))],
        };
        assert_ne!(executor_label(&a), executor_label(&b));
    }
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alba-engine executor_label` → compile error (`executor_label` returns `&'static str`, no docker arm). Red.

- [ ] **Step 3: Implement**

```rust
fn executor_label(kind: &ExecutorKind) -> String {
    match kind {
        ExecutorKind::Shell => "embedded".to_string(),
        ExecutorKind::SystemShell => "system".to_string(),
        ExecutorKind::Docker { image, volumes, workdir } => format!(
            "docker:image={image};volumes={};workdir={}",
            volumes.join(","),
            workdir.as_deref().unwrap_or(""),
        ),
        ExecutorKind::Plugin { name, options } => {
            let options = options.iter()
                .map(|(key, value)| format!("{key}={}", option_label(value)))
                .collect::<Vec<_>>()
                .join(";");
            format!("plugin:{name};{options}")
        }
    }
}

fn option_label(value: &OptionValue) -> String {
    match value {
        OptionValue::Str(v) => v.clone(),
        OptionValue::Bool(v) => v.to_string(),
        OptionValue::List(v) => v.join(","),
    }
}
```

Ripples, all mechanical: `CacheableBeam.executor: String` (built with `executor_label(&task.beam.executor)`); `BeamFacts.executor: &'a str` (fingerprint.rs — update its doc comment: the label now carries configuration, not just `"embedded"`/`"system"`); the `static_contribution` call site passes `&executor_label(..)`. Check `static_contribution`'s signature in `cache/` and relax `&'static str` to `&str` there too. Bump `FORMAT_VERSION` in `cache/store.rs`? **No** — the label for existing shell beams is byte-identical (`"embedded"`/`"system"`), so old manifests stay valid; only beams whose kind changes get new fingerprints, which is the point.

- [ ] **Step 4: Full gate, then commit**

```bash
git add -A
git commit -m "✨ feat(engine): fold executor configuration into the cache key"
```

---

### Task 4: The docker executor

`DockerExecutor` in `alba-executors`: one container per beam via the `docker` CLI, commands through `docker exec`, mounts and path mapping, cancellation, removal on close.

**Files:**
- Create: `crates/alba-executors/src/docker.rs`
- Modify: `crates/alba-executors/src/lib.rs` (module + re-export)
- Modify: `crates/alba-executors/Cargo.toml` (`serde.workspace = true` for the config derive)
- Test: unit tests in `docker.rs`; Create: `crates/alba-executors/tests/docker.rs` (ignored integration tests)

**Interfaces:**
- Consumes: Task 1's `Executor`/`ExecSession`/`BeamContext`, `shell::stream_lines` (`pub(crate)` since Task 1).
- Produces: `pub struct DockerExecutor; impl DockerExecutor { pub fn new(project_root: PathBuf) -> Self }` — `open` deserializes `BeamContext.options` as `{"image": String, "volumes": [String], "workdir": String?}`.

- [ ] **Step 1: Write the failing unit tests**

In `docker.rs`'s `#[cfg(test)]` module (write the tests first; the functions they name are the implementation's skeleton):

```rust
#[test]
fn config_deserializes_with_defaults() {
    let config: DockerConfig = serde_json::from_value(
        serde_json::json!({"image": "alpine:3", "volumes": null, "workdir": null}),
    ).unwrap();
    assert_eq!(config, DockerConfig {
        image: "alpine:3".into(), volumes: vec![], workdir: None,
    });
}

#[test]
fn run_args_mount_the_project_and_keep_the_container_dormant() {
    let config = DockerConfig { image: "alpine:3".into(), volumes: vec!["h:/c".into()], workdir: None };
    let args = run_args(&config, std::path::Path::new("/proj"), "alba-build-1-0", "build");
    #[cfg(unix)]
    assert_eq!(args, vec![
        "run", "-d", "--rm", "--name", "alba-build-1-0", "--label", "alba.beam=build",
        "-v", "/proj:/proj", "-v", "h:/c",
        "alpine:3", "sh", "-c", "sleep 2147483647",
    ]);
    #[cfg(windows)]
    assert!(
        args.iter().any(|arg| arg.ends_with(":/workspace")),
        "the windows mount must target /workspace: {args:?}",
    );
}

#[test]
fn container_cwd_is_identity_on_unix_and_workspace_relative_on_windows() {
    let mapping = PathMapping::new(std::path::Path::new("/proj"), None);
    #[cfg(unix)]
    assert_eq!(mapping.container_cwd(std::path::Path::new("/proj/api")), "/proj/api");
    #[cfg(windows)]
    {
        let mapping = PathMapping::new(std::path::Path::new(r"C:\proj"), None);
        assert_eq!(mapping.container_cwd(std::path::Path::new(r"C:\proj\api")), "/workspace/api");
    }
}

#[test]
fn a_declared_workdir_overrides_the_computed_cwd() {
    let mapping = PathMapping::new(std::path::Path::new("/proj"), Some("/inside".to_string()));
    assert_eq!(mapping.container_cwd(std::path::Path::new("/proj/api")), "/inside");
}

#[test]
fn container_names_are_sanitized_and_unique() {
    let a = container_name("api:build");
    let b = container_name("api:build");
    assert!(a.starts_with("alba-api-build-"));
    assert_ne!(a, b);
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)));
}

#[test]
fn exec_args_carry_workdir_and_env() {
    let args = exec_args("cid", "/proj/api", &[("K".into(), "V".into())], "echo hi");
    assert_eq!(args, vec![
        "exec", "-w", "/proj/api", "-e", "K=V", "cid", "sh", "-c", "echo hi",
    ]);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p alba-executors docker` → compile error (module does not exist). Red.

- [ ] **Step 3: Implement `docker.rs`**

Module doc comment: one container per beam session, kept alive by a `sh -c "sleep 2147483647"` dormant process (documented prerequisite: the image provides `/bin/sh`); the `docker` CLI, not an API client, for Docker Desktop/context/lookalike compatibility; `--rm` so a crashed Alba still leaves cleanup to the daemon.

```rust
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::process::Command;

use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor,
    OutputLine, Stream,
};

const STOP_GRACE: Duration = Duration::from_secs(5);

pub struct DockerExecutor {
    project_root: PathBuf,
}

impl DockerExecutor {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Debug, PartialEq, serde::Deserialize)]
struct DockerConfig {
    image: String,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    workdir: Option<String>,
}
```

Key pieces (each already pinned by a unit test):

- `fn container_name(beam: &str) -> String` — `alba-<sanitized>-<pid>-<counter>` with a `static COUNTER: AtomicU64`; sanitize by mapping every char outside `[A-Za-z0-9_.-]` to `-`.
- `struct PathMapping { project_root: PathBuf, workdir: Option<String> }` with `fn container_cwd(&self, cwd: &Path) -> String`: the declared `workdir` wins; else on unix the host path verbatim; on windows `/workspace/<cwd relative to project_root, `/`-separated>` (fall back to `/workspace` if `cwd` is not under the root).
- `fn run_args(config, project_root, name, beam) -> Vec<String>` — as the unit test spells out; the mount target is the root itself on unix, `/workspace` on windows.
- `fn exec_args(container: &str, cwd: &str, env: &[(String, String)], command: &str) -> Vec<String>`.

`Executor::open`:

1. `serde_json::from_value::<DockerConfig>(beam.options)` → `ExecError` on mismatch (message: `invalid docker options: {error}`).
2. Spawn `docker` with `run_args`, stdout+stderr piped, stdin null. A spawn error maps to `` ExecError { message: format!("cannot run `docker`: {error} — the docker executor requires Docker on the PATH") } ``.
3. While waiting for it: forward stderr lines (pull progress) through `stream_lines(stderr, Stream::Stderr, beam.output.clone())` as a spawned task; race `child.wait()` against `beam.cancel.cancelled()` — on cancellation kill the child, best-effort `docker rm -f <name>` (a fire-and-forget `Command` with `Stdio::null()` everywhere), and return `ExecError { message: "container start cancelled".into() }`.
4. Non-zero exit → `ExecError { message: format!("docker run exited with code {code} for image `{image}`") }` (the pull/daemon error text already streamed to the beam's output).
5. Zero exit → the container id is the trimmed stdout; return `DockerSession { container: id, name, mapping }`.

`ExecSession for DockerSession`:

- `execute`: build `exec_args(container, &mapping.container_cwd(&cmd.cwd), &cmd.env, &cmd.command)`; spawn with piped stdout/stderr; stream both via `stream_lines` tasks into `ctx.output`; race `child.wait()` against `ctx.cancel.cancelled()` — on cancellation run `docker stop --time 5 <container>` then `docker rm -f <container>` (both quiet), then `child.wait()` with a `STOP_GRACE` timeout falling back to `child.kill()`; join both stream tasks; exit code `status.code().unwrap_or(-1)`.
- `close`: quiet `docker rm -f <container>`; a non-zero exit maps to `ExecError { message: format!("docker rm exited with code {code}") }` (the engine downgrades close errors to a notice).

- [ ] **Step 4: Run the unit tests**

Run: `cargo test -p alba-executors docker` → PASS.

- [ ] **Step 5: Write the ignored integration tests**

`crates/alba-executors/tests/docker.rs` — every test `#[ignore = "requires a running docker daemon"]`, image `alpine:3` throughout, session built by hand:

```rust
fn beam_context(options: serde_json::Value) -> (BeamContext, UnboundedReceiver<OutputLine>) { .. }

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn runs_a_command_in_the_declared_image_and_reports_its_exit_code() {
    // open with {"image":"alpine:3"}; execute `echo hello from docker`
    // (cwd: the project root used to build the executor, a tempdir);
    // assert the stdout line and exit_code 0; execute `sh -c "exit 7"`
    // → exit_code 7; close.
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn container_state_persists_across_commands_of_one_session() {
    // execute `echo state > /tmp/probe` then `cat /tmp/probe` in the same
    // session; assert the second command sees "state" — the one-container-
    // per-beam behavior the whole design decision is about.
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_project_mount_makes_host_files_visible() {
    // Write hello.txt in the tempdir project root; execute `cat hello.txt`
    // with cwd = project root; assert its content comes back.
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn close_removes_the_container() {
    // open, close, then spawn `docker ps -aq --filter label=alba.beam=<beam>`
    // and assert empty output.
}
```

- [ ] **Step 6: Run them against a real daemon**

Run: `cargo test -p alba-executors --test docker -- --ignored` (requires local docker; if unavailable, state that explicitly in the task report — CI's linux job covers them after Task 11). Expected: PASS.

- [ ] **Step 7: Full gate, then commit**

```bash
git add -A
git commit -m "✨ feat(executors): run beams in docker containers"
```

---

### Task 5: Docker dispatch in the engine and the CLI

`Executors` gains a `docker` slot, `plan` stops rejecting docker beams, the options JSON carries the docker configuration, and the CLI constructs `DockerExecutor` with the project root.

**Files:**
- Modify: `crates/alba-engine/src/scheduler.rs`
- Modify: `crates/alba-engine/tests/scheduler.rs`
- Modify: `crates/alba-cli/src/commands/run.rs`
- Test: `crates/alba-cli/tests/cli_plugin.rs` does not exist yet; the docker e2e goes in a new `crates/alba-cli/tests/cli_docker.rs`

**Interfaces:**
- Consumes: Task 4's `DockerExecutor::new(PathBuf)`.
- Produces: `Executors { pub embedded, pub system, pub docker: Arc<dyn Executor> }`; `Executors::uniform` fills all three.

- [ ] **Step 1: Write the failing engine test**

In `crates/alba-engine/tests/scheduler.rs`:

```rust
#[tokio::test]
async fn a_docker_beam_dispatches_to_the_docker_slot_with_its_options() {
    // Beamfile: beam ship { executor docker { image "alpine:3" workdir "/w" } run "deploy" }
    // Executors { embedded: fake_a, system: fake_a, docker: fake_b }.
    // Assert fake_b.events() == [Opened { beam: "ship", options }, Executed, Closed]
    // where options == json!({"image": "alpine:3", "volumes": [], "workdir": "/w"}),
    // and fake_a.events() is empty.
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p alba-engine --test scheduler docker_beam` → compile error (`Executors` has no `docker` field). Red.

- [ ] **Step 3: Implement the engine side**

`scheduler.rs`:
- `Executors` gains `pub docker: Arc<dyn Executor>`; `uniform` clones into all three; `for_beam`'s `Docker` arm returns `Arc::clone(&self.docker)` (delete the `unreachable!` and the stale doc comment).
- Delete the docker rejection in `plan` (keep the plugin one).
- `executor_options` gains the real docker arm:

```rust
ExecutorKind::Docker { image, volumes, workdir } => serde_json::json!({
    "image": image,
    "volumes": volumes,
    "workdir": workdir,
}),
```
- Update `run`'s doc comment (an `Err` is no longer "a docker beam").

- [ ] **Step 4: Wire the CLI**

`commands/run.rs`: the three `Executors { .. }` literals (lines ~175, ~275, ~388) become calls to one helper defined in that file:

```rust
/// Every run's executor set. Docker mounts the project at the root
/// Beamfile's directory, resolved absolutely so the mount stays correct
/// whatever the process's cwd does afterwards.
fn executors(beamfile: &Path) -> Executors {
    let project_root = std::path::absolute(beamfile)
        .unwrap_or_else(|_| beamfile.to_path_buf())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Executors {
        embedded: Arc::new(EmbeddedShellExecutor),
        system: Arc::new(SystemShellExecutor),
        docker: Arc::new(DockerExecutor::new(project_root)),
    }
}
```

(import `DockerExecutor` from `alba_executors`; each call site has `beamfile` in scope).

- [ ] **Step 5: Write the ignored docker e2e**

Create `crates/alba-cli/tests/cli_docker.rs` following `cli_run.rs`'s tempdir + `assert_cmd` style:

```rust
#[test]
#[ignore = "requires a running docker daemon"]
fn a_docker_beam_runs_its_command_in_the_image() {
    // Beamfile:
    //   beam hello {
    //     executor docker { image "alpine:3" }
    //     run "echo hello from a container"
    //   }
    // alba run hello → exit 0, stdout contains "hello from a container".
}

#[test]
#[ignore = "requires a running docker daemon"]
fn a_missing_image_fails_the_beam_not_the_run_machinery() {
    // executor docker { image "alba-definitely-does-not-exist:latest" }
    // alba run → exit 1 (a beam failure), stderr/stdout mentions the beam
    // as failed; not exit 2.
}
```

- [ ] **Step 6: Run everything**

Run: `cargo test -p alba-engine --test scheduler docker_beam` → PASS. Full gate. If docker is available locally, also `cargo test -p alba-cli --test cli_docker -- --ignored`.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "✨ feat(engine): dispatch docker beams to a real container executor"
```

---

### Task 6: The plugin wire protocol

The message types and their exact JSON encoding, as a public module of `alba-executors` (the CLI's conformance checker reuses it).

**Files:**
- Create: `crates/alba-executors/src/protocol.rs`
- Modify: `crates/alba-executors/src/lib.rs` (`pub mod protocol;`)

**Interfaces:**
- Produces:

```rust
// alba-executors/src/protocol.rs
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Open { protocol: u32, beam: String, dir: String, options: serde_json::Value },
    Execute { command: String, env: Vec<(String, String)>, cwd: String },
    Cancel,
    Close,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    Ready,
    Output { stream: WireStream, text: String },
    Exit { code: i32 },
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireStream { Stdout, Stderr }
```

- [ ] **Step 1: Write the failing tests**

In `protocol.rs`'s `#[cfg(test)]` module — the wire strings are the contract, so pin them byte-for-byte:

```rust
#[test]
fn the_wire_encoding_matches_the_published_specification() {
    let open = HostMessage::Open {
        protocol: PROTOCOL_VERSION,
        beam: "deploy".into(),
        dir: "/abs/path".into(),
        options: serde_json::json!({"image": "x"}),
    };
    assert_eq!(
        serde_json::to_string(&open).unwrap(),
        r#"{"type":"open","protocol":1,"beam":"deploy","dir":"/abs/path","options":{"image":"x"}}"#,
    );
    let execute = HostMessage::Execute {
        command: "echo hi".into(),
        env: vec![("K".into(), "V".into())],
        cwd: "/abs/path".into(),
    };
    assert_eq!(
        serde_json::to_string(&execute).unwrap(),
        r#"{"type":"execute","command":"echo hi","env":[["K","V"]],"cwd":"/abs/path"}"#,
    );
    assert_eq!(serde_json::to_string(&HostMessage::Cancel).unwrap(), r#"{"type":"cancel"}"#);
    assert_eq!(serde_json::to_string(&HostMessage::Close).unwrap(), r#"{"type":"close"}"#);
}

#[test]
fn plugin_messages_decode_from_their_wire_form() {
    assert_eq!(
        serde_json::from_str::<PluginMessage>(r#"{"type":"ready"}"#).unwrap(),
        PluginMessage::Ready,
    );
    assert_eq!(
        serde_json::from_str::<PluginMessage>(
            r#"{"type":"output","stream":"stderr","text":"warm"}"#,
        ).unwrap(),
        PluginMessage::Output { stream: WireStream::Stderr, text: "warm".into() },
    );
    assert_eq!(
        serde_json::from_str::<PluginMessage>(r#"{"type":"exit","code":3}"#).unwrap(),
        PluginMessage::Exit { code: 3 },
    );
}

#[test]
fn an_unknown_message_type_fails_to_decode() {
    assert!(serde_json::from_str::<PluginMessage>(r#"{"type":"surprise"}"#).is_err());
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p alba-executors protocol` → compile error. Red.

- [ ] **Step 3: Implement** — the enums from Interfaces plus a module doc comment: one JSON object per line on stdin/stdout, host speaks `HostMessage`, plugin answers `PluginMessage`, stderr is free-form and relayed to the beam's output.

- [ ] **Step 4: Run, gate, commit**

```bash
git add -A
git commit -m "✨ feat(executors): define the plugin wire protocol"
```

---

### Task 7: The plugin executor

`PluginExecutor` spawns `alba-executor-<name>` binaries and speaks the protocol; tested against an in-crate scripted fake plugin binary.

**Files:**
- Create: `crates/alba-executors/src/plugin.rs`
- Create: `crates/alba-executors/tests/support/fake_plugin.rs`
- Modify: `crates/alba-executors/Cargo.toml` (`[[bin]] name = "fake-plugin", path = "tests/support/fake_plugin.rs"`)
- Modify: `crates/alba-executors/src/lib.rs` (`mod plugin; pub use plugin::PluginExecutor;`)
- Test: `crates/alba-executors/tests/plugin.rs`

**Interfaces:**
- Consumes: Task 6's `protocol` module; `shell::stream_lines`.
- Produces:

```rust
pub struct PluginExecutor { /* binary, handshake_timeout, grace */ }
impl PluginExecutor {
    pub fn new(binary: PathBuf) -> Self;               // 10s handshake, 5s grace
    pub fn with_timeouts(binary: PathBuf, handshake: Duration, grace: Duration) -> Self; // tests
}
```

- [ ] **Step 1: Write the fake plugin binary**

`tests/support/fake_plugin.rs` — a `#[tokio::main]` binary whose mode comes from `FAKE_PLUGIN_MODE` (default `ok`):

- `ok`: answer `ready`; on `execute`, interpret the command — `echo <text>` → one stdout `output` + `exit 0`; `fail <code>` → `exit <code>`; `stderr-probe` → write a raw line to the process's stderr then `exit 0`; `hang` → wait for the next stdin message: `cancel` → `exit -1`, anything else ignored; on `close` → exit the process.
- `mute`: read stdin forever, never write anything.
- `garbage`: print `this is not json` on stdout, then behave like `mute`.
- `refuse`: answer `{"type":"error","message":"protocol 1 not supported"}` to `open`.
- `die`: exit with code 3 immediately, before answering anything.

Implement it with `tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt}` and `serde_json` on `alba_executors::protocol::{HostMessage, PluginMessage, WireStream}` (a bin of the crate can `use alba_executors::...`).

- [ ] **Step 2: Write the failing executor tests**

`tests/plugin.rs` — the binary's path is `env!("CARGO_BIN_EXE_fake-plugin")`. The fake plugin's misbehavior mode cannot ride the protocol (the `mute`/`garbage`/`die` modes misbehave before the handshake, so no `open` message can select them) and cannot ride an environment variable (parallel tests mutating the test process's environment race each other). The mode therefore travels as the spawned binary's **first command-line argument** (`fake-plugin garbage`, default `ok`), which requires one test-facing extension on the executor:

```rust
pub struct PluginExecutor {
    binary: PathBuf,
    args: Vec<String>,
    handshake_timeout: Duration,
    grace: Duration,
}
impl PluginExecutor {
    pub fn new(binary: PathBuf) -> Self;  // args: [], 10s, 5s
    pub fn with_timeouts(binary: PathBuf, handshake: Duration, grace: Duration) -> Self;
    /// Extra argv for the spawned plugin. Alba itself never sets any; the
    /// test suite uses it to select a fake plugin's scripted behavior.
    pub fn with_args(mut self, args: Vec<String>) -> Self;
}
```

The fake plugin takes the mode as `argv[1]` (default `ok`). Tests:

```rust
#[tokio::test]
async fn handshake_execute_and_close_roundtrip() {
    // mode ok; execute "echo ping" → one stdout OutputLine "ping",
    // ExecResult { exit_code: 0 }; execute "fail 3" → exit_code 3;
    // close() → Ok.
}

#[tokio::test]
async fn plugin_stderr_lands_in_the_beams_output_as_stderr() {
    // mode ok; execute "stderr-probe"; assert an OutputLine with
    // Stream::Stderr arrives on the BeamContext output channel.
}

#[tokio::test]
async fn a_mute_plugin_fails_the_handshake_within_the_timeout() {
    // mode mute, handshake 500ms; open() → Err whose message mentions
    // the handshake; the child process is killed (waitable quickly).
}

#[tokio::test]
async fn a_garbage_line_is_a_protocol_error_quoting_the_line() {
    // mode garbage; open() → Err, message contains "this is not json".
}

#[tokio::test]
async fn a_refusing_plugin_surfaces_its_own_error_message() {
    // mode refuse; open() → Err, message contains "protocol 1 not supported".
}

#[tokio::test]
async fn a_dead_plugin_reports_its_exit_code() {
    // mode die; open() → Err, message contains "exited" and "3".
}

#[tokio::test]
async fn cancel_asks_first_then_kills_after_the_grace() {
    // mode ok; execute "hang" with a token cancelled after 50ms →
    // the fake answers exit -1 on the cancel message: execute returns
    // Ok(ExecResult { exit_code: -1 }) well before the grace expires.
}

#[tokio::test]
async fn a_cancel_deaf_plugin_is_killed_after_the_grace() {
    // mode mute cannot get past open; use mode ok with command "hang"
    // and FAKE deafness: add mode "deaf" to the fake (ready, then on
    // execute reads and discards everything, answers nothing). Cancel →
    // execute returns Ok(exit_code: -1) after ~grace (300ms), not hang.
}
```

(Add the `deaf` mode to the fake plugin: `ready` at open, then silence.)

- [ ] **Step 3: Run to verify they fail** — `cargo test -p alba-executors --test plugin` → compile error (no `plugin` module). Red.

- [ ] **Step 4: Implement `plugin.rs`**

```rust
pub struct PluginSession {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    grace: Duration,
}
```

- `open`: spawn (`stdin`/`stdout` piped, `stderr` piped); spawn a detached task `stream_lines(stderr, Stream::Stderr, beam.output.clone())`; send `HostMessage::Open { protocol: PROTOCOL_VERSION, beam, dir: beam.dir.display().to_string(), options }` as one line; await one decoded `PluginMessage` under `handshake_timeout`:
  - `Ready` → session; `Error { message }` → kill child, `ExecError { message }`;
  - timeout → kill, `ExecError` "plugin did not answer the handshake within {n}s";
  - EOF → `child.wait()`, `ExecError` "plugin exited with code {code} before answering the handshake";
  - undecodable line → kill, `ExecError` "plugin spoke a non-protocol line: `{line}`".
- Reading helper shared by open/execute: `async fn read_message(&mut self) -> Result<PluginMessage, ExecError>` — `self.stdout.next_line()`, decode with `serde_json::from_str`, quote the offending line on failure, and map EOF to the exited-with-code error.
- `execute`: send `Execute { command, env, cwd: cmd.cwd.display().to_string() }`; loop on messages, forwarding `Output` into `ctx.output` (map `WireStream` → `Stream`), returning on `Exit { code }` → `Ok(ExecResult { exit_code: code })`, `Error` → `Err`. Cancellation: `tokio::select!` between `read_message` and `ctx.cancel.cancelled()` — on cancellation send `Cancel` once, then keep reading with an overall `grace` timeout; on timeout `child.start_kill()` and return `Ok(ExecResult { exit_code: -1 })` (the engine's token check classifies it as cancelled).
- `close`: send `Close`; `tokio::time::timeout(grace, child.wait())`; timeout → `start_kill` + wait, `Ok(())` either way a wait succeeds; I/O failure → `ExecError`.

- [ ] **Step 5: Run, gate, commit**

`cargo test -p alba-executors --test plugin` → PASS. Full gate.

```bash
git add -A
git commit -m "✨ feat(executors): speak the wire protocol to external plugin binaries"
```

---

### Task 8: Plugin resolution and dispatch in the engine

Plan-time PATH resolution of `alba-executor-<name>` with typo suggestions; per-beam `PluginExecutor` dispatch; the temporary rejection from Task 2 disappears.

**Files:**
- Modify: `crates/alba-core/src/eval.rs` + `crates/alba-core/src/lib.rs` (`suggest` goes `pub`, exported)
- Modify: `crates/alba-engine/Cargo.toml` (`which.workspace = true`)
- Modify: `crates/alba-engine/src/scheduler.rs`
- Test: `crates/alba-engine/tests/scheduler.rs` + a unit test in `scheduler.rs`

**Interfaces:**
- Consumes: Task 7's `PluginExecutor::new(PathBuf)`; `alba_core::suggest(target, candidates)` (newly public, unchanged signature).
- Produces: `plan` returns `Result<(Vec<&Beam>, HashMap<String, PathBuf>), EngineError>`; scheduler-private `fn resolve_plugins(beams: &[&Beam], lookup: impl Fn(&str) -> Option<PathBuf>) -> Result<HashMap<String, PathBuf>, EngineError>`.

- [ ] **Step 1: Write the failing tests**

Unit test in `scheduler.rs`'s test module:

```rust
#[test]
fn a_missing_plugin_names_the_search_and_suggests_the_typo() {
    let project = alba_core::load_str(
        r#"beam d { executor dokcer { image "x" } run "x" }"#,
    ).unwrap();
    let beams: Vec<&Beam> = project.beams.iter().collect();
    let err = resolve_plugins(&beams, |_| None).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("`dokcer` is neither a built-in executor nor `alba-executor-dokcer` on the PATH"), "{message}");
    assert!(message.contains("did you mean `docker`?"), "{message}");
}

#[test]
fn resolution_collects_one_path_per_distinct_plugin_name() {
    // Two beams on executor `podman`, one on `remote`; lookup returns
    // Some(PathBuf::from(format!("/bin/{binary}"))); assert the map has
    // exactly {"podman": "/bin/alba-executor-podman", "remote": "/bin/alba-executor-remote"}.
}
```

End-to-end through `run` in `tests/scheduler.rs`:

```rust
#[tokio::test]
async fn a_run_with_an_unresolvable_plugin_fails_before_starting_anything() {
    // beam d { executor definitely-not-installed run "x" } — hmm: executor
    // names are identifiers; use `executor notinstalled`. run(..) →
    // Err(EngineError::Unschedulable), fake executor saw no events.
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p alba-engine resolve_plugins` → compile error. Red.

- [ ] **Step 3: Implement**

`alba-core`: change `pub(crate) fn suggest` to `pub fn suggest` with a short doc comment ("nearest candidate by edit distance, for did-you-mean help — shared with the engine's plugin resolution"); add it to `lib.rs`'s `pub use eval::{...}`.

`scheduler.rs`:

```rust
/// Resolves every distinct plugin name in the subgraph to its binary, or
/// fails the run before anything starts. `lookup` receives the binary
/// name (`alba-executor-<name>`) and answers with its path; production
/// passes a `which` lookup, tests inject their own.
fn resolve_plugins(
    beams: &[&Beam],
    lookup: impl Fn(&str) -> Option<PathBuf>,
) -> Result<HashMap<String, PathBuf>, EngineError> {
    let mut resolved = HashMap::new();
    for beam in beams {
        let ExecutorKind::Plugin { name, .. } = &beam.executor else {
            continue;
        };
        if resolved.contains_key(name.as_str()) {
            continue;
        }
        let binary = format!("alba-executor-{name}");
        match lookup(&binary) {
            Some(path) => {
                resolved.insert(name.clone(), path);
            }
            None => {
                let mut message = format!(
                    "beam `{}` uses executor `{name}`: `{name}` is neither a built-in \
                     executor nor `{binary}` on the PATH",
                    beam.id.0,
                );
                if let Some(candidate) =
                    alba_core::suggest(name, ["shell", "system_shell", "docker"].into_iter())
                {
                    message.push_str(&format!(" — did you mean `{candidate}`?"));
                }
                return Err(EngineError::Unschedulable(message));
            }
        }
    }
    Ok(resolved)
}
```

`plan`: drop the Task 2 rejection; return `(beams, resolve_plugins(&beams, |binary| which::which(binary).ok())?)`. In `run`, destructure `let (beams, plugins) = plan(..)?` and build each task's executor:

```rust
executor: match &beam.executor {
    ExecutorKind::Plugin { name, .. } => Arc::new(PluginExecutor::new(
        plugins[name.as_str()].clone(),
    )),
    kind => executors.for_beam(kind),
},
```

`executor_options`'s `Plugin` arm becomes the verbatim options object:

```rust
ExecutorKind::Plugin { options, .. } => serde_json::Value::Object(
    options.iter().map(|(key, value)| (key.clone(), option_json(value))).collect(),
),
```

```rust
fn option_json(value: &OptionValue) -> serde_json::Value {
    match value {
        OptionValue::Str(v) => serde_json::Value::String(v.clone()),
        OptionValue::Bool(v) => serde_json::Value::Bool(*v),
        OptionValue::List(v) => v.iter().cloned().map(serde_json::Value::String).collect(),
    }
}
```

Delete `for_beam`'s and `executor_label`'s plugin `unreachable!` (the label arm was implemented in Task 3 — only `for_beam` still has one; it becomes genuinely unreachable *for plugins* since dispatch happens before `for_beam`, so change `for_beam` to only ever receive built-in kinds and say so in its doc comment).

- [ ] **Step 4: Run, gate, commit**

```bash
git add -A
git commit -m "✨ feat(engine): resolve and dispatch plugin executors from the PATH"
```

---

### Task 9: The example plugin, end to end

A std-only workspace binary crate that implements the protocol, proving a plugin needs no async runtime — and the CLI e2e that runs a Beamfile through it.

**Files:**
- Create: `crates/alba-executor-example/Cargo.toml`
- Create: `crates/alba-executor-example/src/main.rs`
- Test: `crates/alba-cli/tests/cli_plugin.rs`

**Interfaces:**
- Consumes: the wire protocol (by hand — deliberately not the `protocol` module, to prove the wire is the contract; depends only on `serde_json`).
- Produces: a binary named `alba-executor-example` understanding commands `echo <text>`, `fail <code>`, `sleep <ms>` (cancellable), anything else → stderr `unknown command` + exit 127.

- [ ] **Step 1: Write the failing e2e**

`crates/alba-cli/tests/cli_plugin.rs` (follow `cli_run.rs`'s helpers):

```rust
use assert_cmd::Command;

/// The example plugin's directory, prepended to PATH so `alba` resolves
/// `alba-executor-example` the way a user install would.
fn plugin_path() -> (std::ffi::OsString, tempfile::TempDir) { .. }
// Build PATH = <dir of assert_cmd::cargo::cargo_bin("alba-executor-example")>
//              + platform separator + existing PATH,
// via std::env::join_paths.

#[test]
fn a_plugin_beam_runs_end_to_end() {
    // Beamfile:
    //   beam hello {
    //     executor example
    //     run "echo carried by the example plugin"
    //   }
    // alba run hello (with the augmented PATH) → exit 0, stdout contains
    // "carried by the example plugin".
}

#[test]
fn a_plugin_beams_failure_is_an_ordinary_beam_failure() {
    // run "fail 3" → alba exits 1; the summary reports the beam failed.
}

#[test]
fn a_missing_plugin_is_a_clean_plan_time_error() {
    // executor nosuchthing, WITHOUT the augmented PATH → exit 2, stderr
    // contains "neither a built-in executor nor `alba-executor-nosuchthing`".
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p alba-cli --test cli_plugin` → the first two fail (no such binary), the third may already pass — keep it, it pins Task 8's CLI-visible message.

- [ ] **Step 3: Implement the example plugin**

`Cargo.toml`:

```toml
[package]
name = "alba-executor-example"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde_json.workspace = true
```

`main.rs` — std only. One thread reads stdin lines into an `mpsc::Sender<serde_json::Value>`; the main loop:

```rust
// on {"type":"open", "protocol": p, ..}: p == 1 → {"type":"ready"},
//   else {"type":"error","message":"protocol {p} not supported"} and exit.
// on {"type":"execute","command":c,..}: match the first word of c —
//   "echo"  → {"type":"output","stream":"stdout","text": rest} then {"type":"exit","code":0}
//   "fail"  → {"type":"exit","code": rest parsed as i32 (default 1)}
//   "sleep" → recv_timeout(rest ms) on the message channel:
//               a {"type":"cancel"} within the window → {"type":"exit","code":-1}
//               timeout → {"type":"exit","code":0}
//   _       → {"type":"output","stream":"stderr","text":"unknown command"} + {"type":"exit","code":127}
// on {"type":"close"}: exit(0).
```

Every reply is one `serde_json::json!` line printed to stdout and flushed. Comment the file as the reference implementation the protocol documentation points at.

- [ ] **Step 4: Run, gate, commit**

`cargo test -p alba-cli --test cli_plugin` → PASS (note: `cargo test --workspace` builds all workspace bins, so `cargo_bin` finds the example). Full gate.

```bash
git add -A
git commit -m "✨ feat(plugins): ship the example executor and run it end to end"
```

---

### Task 10: `alba plugin check`

The conformance subcommand: drives any binary through the protocol and prints a ✓/✗ report.

**Files:**
- Modify: `crates/alba-cli/src/args.rs`
- Modify: `crates/alba-cli/src/main.rs`
- Modify: `crates/alba-cli/src/commands/mod.rs`
- Create: `crates/alba-cli/src/commands/plugin.rs`
- Test: `crates/alba-cli/tests/cli_plugin.rs`

**Interfaces:**
- Consumes: `PluginExecutor::{new, with_timeouts}` and the `protocol` module from `alba-executors`.
- Produces: `alba plugin check <BINARY> [--command <CMD>] [--cancel-command <CMD>]`; exit 0 conformant, 1 non-conformant, 2 for Alba's own failures (binary path does not exist).

- [ ] **Step 1: Write the failing e2e**

Append to `cli_plugin.rs`:

```rust
#[test]
fn plugin_check_passes_the_example_plugin() {
    // alba plugin check <path to alba-executor-example>
    //   --command "echo probe" --cancel-command "sleep 30000"
    // → exit 0; stdout contains "✓ handshake", "✓ execute", "✓ cancel",
    //   "✓ close", and "conformant".
}

#[cfg(unix)]
#[test]
fn plugin_check_reports_a_mute_binary() {
    // Write a script: "#!/bin/sh\nsleep 30\n", chmod +x.
    // alba plugin check <script> → exit 1; stdout contains "✗ handshake".
    // (unix-only: the equivalent windows shim is not worth the ceremony;
    // the misbehavior matrix is covered by alba-executors' plugin tests.)
}

#[test]
fn plugin_check_rejects_a_missing_binary_as_an_alba_error() {
    // alba plugin check /no/such/binary → exit 2.
}
```

- [ ] **Step 2: Run to verify they fail** — `cargo test -p alba-cli --test cli_plugin plugin_check` → clap error (no such subcommand). Red.

- [ ] **Step 3: Implement**

`args.rs`:

```rust
// in Command:
    /// Work with executor plugins.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Drive an executor plugin binary through protocol v1 and report
    /// whether it conforms.
    Check {
        /// Path to the plugin binary to check.
        #[arg(value_name = "BINARY")]
        binary: PathBuf,
        /// The command sent in the execution check.
        #[arg(long, value_name = "CMD", default_value = "echo alba-plugin-check")]
        command: String,
        /// A long-running command for the cancellation check; the plugin
        /// must answer the cancel within the 5-second grace.
        #[arg(long, value_name = "CMD", default_value = "sleep 30")]
        cancel_command: String,
    },
}
```

`main.rs`: dispatch `Some(Command::Plugin { command })` right next to the `Cache` early dispatch — before the Beamfile loads (a conformance check needs no Beamfile):

```rust
if let Some(Command::Plugin { command }) = &cli.command {
    return commands::plugin::run(command);
}
```

(and the corresponding `unreachable!` arm in the later match, mirroring `Cache`).

`commands/plugin.rs`: a sync `pub fn run(command: &PluginCommand) -> i32` that builds a tokio runtime (`tokio::runtime::Runtime::new()`, mirroring however `commands::run` does it — read it first and match), then four checks, each printing through `LineSink::stdout()`:

1. **handshake** — binary exists (`is_file`, else print the message and return `EXIT_ALBA_ERROR`); `PluginExecutor::new(binary)` + `open` with `BeamContext { beam: "plugin-check", dir: current dir, options: Null, output, cancel }` → ✓ or ✗ with the error's message.
2. **execute** — on the open session, `execute` the `--command` under a 30s timeout; any `Ok(ExecResult { .. })` is a ✓ (the exit code is the plugin's business; the protocol conformance is that an `exit` message arrived); report the observed exit code and output line count.
3. **cancel** — a **fresh** session (fresh process): `execute(--cancel-command)` with a token cancelled after 100ms; `Ok` within grace + 1s → ✓; an `Err` or a hang past it → ✗.
4. **close** — `close()` on that session → ✓/✗.

Print a final line: `conformant: protocol v1` (all ✓) or `not conformant` and return 0 / 1. Each ✗ carries the failure detail on the same line.

- [ ] **Step 4: Run, gate, commit**

```bash
git add -A
git commit -m "✨ feat(cli): check a plugin binary's protocol conformance"
```

---

### Task 11: CI runs the docker tests on linux

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Verify the ignored-test inventory**

Run: `grep -rn '#\[ignore' crates/ --include='*.rs'` — expected: only the docker tests from Tasks 4 and 5. If anything else shows up, scope the CI invocation with a `docker` name filter instead of bare `--ignored`.

- [ ] **Step 2: Add the step**

After the `Test` step in `ci.yml`:

```yaml
      # The docker executor's integration tests are `#[ignore]`d because
      # they need a running daemon; the ubuntu runner has one, the macOS
      # and windows runners do not.
      - name: Test (docker executor)
        if: matrix.os == 'ubuntu-latest'
        run: cargo test --workspace --all-features -- --ignored
```

- [ ] **Step 3: Verify and commit**

Local check: `cargo test --workspace --all-features -- --ignored` with docker running (or note it for CI to prove). Push and watch the linux job run the docker tests (`gh run watch` after pushing, if pushing is in scope for the session; otherwise state that CI verification is pending the next push).

```bash
git add .github/workflows/ci.yml
git commit -m "👷 ci(github): run the docker executor tests on the linux job"
```

---

### Task 12: Documentation

**Files:**
- Create: `docs/plugin-protocol.md`
- Modify: `README.md`

- [ ] **Step 1: Write the protocol specification**

`docs/plugin-protocol.md` — the document a third-party author implements from, with no access to Alba's source. Sections:

- **Discovery**: `executor <name> { ... }` for any non-built-in name resolves `alba-executor-<name>` on the PATH at plan time; options are forwarded verbatim, validation is the plugin's job.
- **Lifecycle**: one process per beam; stdin/stdout carry one JSON object per line; stderr is free-form and relayed into the beam's output.
- **Messages**: the full message table with one wire example each (copy the exact strings pinned by Task 6's tests — they are the normative encoding, including `env` as an array of two-element arrays).
- **Versioning**: `protocol: 1`; unsupported → answer `error` and expect termination.
- **Cancellation**: `cancel` during an execute; answer with `exit` promptly; a deaf plugin is killed after a 5-second grace.
- **Timeouts**: 10 seconds to answer the handshake.
- **Conformance**: `alba plugin check <binary>`, with the `--command`/`--cancel-command` knobs; a pointer to `crates/alba-executor-example` as the reference implementation.

- [ ] **Step 2: Update the README**

Read the README first and follow its structure and voice. Add: a docker executor section (the DSL block with `image`/`volumes`/`workdir`, the one-container-per-beam semantics, the `/bin/sh` prerequisite, mount behavior on unix vs windows); a plugins section (PATH discovery, a pointer to `docs/plugin-protocol.md`, `alba plugin check`); update any executor enumeration that still says docker is "not yet supported".

- [ ] **Step 3: Check and commit**

`cargo test --workspace --all-features` one last time (e2e assertions sometimes pin README examples).

```bash
git add -A
git commit -m "📝 docs: document the docker executor and the plugin protocol"
```

---

## Self-review notes (already applied)

- Spec coverage: session contract (T1), DSL extension + plugin admission (T2), cache label (T3), docker executor (T4) and its dispatch/mounting/e2e (T5), wire protocol (T6), plugin executor incl. every robustness case — handshake timeout, garbage, refusal, premature death, deaf cancel (T7), plan-time resolution + suggestion (T8), example plugin + e2e (T9), `alba plugin check` (T10), CI linux docker leg (T11), `docs/plugin-protocol.md` + README (T12). `alba check` needs no code change (the `!= Shell` filter already skips docker/plugin beams) — T2 updates its comment.
- Type consistency: `BeamContext { beam, dir, options, output, cancel }` identical in T1 (definition), T4 (docker consumes `options`), T7 (plugin forwards `options`, uses `dir`), T10 (the checker builds one). `Executors { embedded, system, docker }` defined in T5, consumed in T5's CLI helper. `executor_label -> String` defined in T3, untouched after. `resolve_plugins` defined and consumed only in T8. `PluginExecutor::{new, with_timeouts, with_args}` defined T7, consumed T8 (`new`) and T10 (`new`).
- The one design wrinkle found while planning: selecting the fake plugin's misbehavior mode cannot ride the protocol (pre-handshake modes) nor the environment (parallel tests race) — resolved with `PluginExecutor::with_args`, documented as test-only in its doc comment (T7 records the reasoning inline).
- `FORMAT_VERSION` deliberately not bumped in T3: shell labels are byte-identical, so existing cache entries stay valid.
- Windows: docker tests never run there (CI gate); the path mapping unit tests cover the `/workspace` rewrite under `#[cfg(windows)]`; plugin tests run on all three platforms (the fake plugin is a portable tokio binary).
