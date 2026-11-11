//! Integration tests for [`alba_executors::ComposeExecutor`] against a real
//! docker daemon with the Compose plugin. Every test is `#[ignore]`d (see the
//! reason string on each) because they require `docker` on the `PATH` and a
//! running daemon; run them explicitly with `cargo test -p alba-executors
//! --test compose -- --ignored`.
//!
//! Every test works in its own throwaway Compose project ([`ComposeProject`]),
//! named after the test and the process id so it never collides with another
//! test, another run, or a developer's own project. Dropping it tears that
//! project down and removes every container a session of the test labelled,
//! even when the test panics mid-assertion.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alba_executors::{
    BeamContext, CommandSpec, ComposeExecutor, ExecContext, Executor, OutputLine,
};
use tokio::sync::mpsc::UnboundedReceiver;

/// A throwaway Compose project in a temp directory. Its project name,
/// `alba-it-<suffix>-<pid>`, is also the name of the beam a test opens on it
/// (see [`ComposeProject::beam`]). Dropping it runs
/// `docker compose --profile "*" down -v --rmi local` on that project, which
/// also removes the images Compose built for it, and removes every container
/// labelled for one of its beams.
struct ComposeProject {
    dir: tempfile::TempDir,
    name: String,
    /// The Compose file, relative to `dir`, when it is not found by
    /// Compose's own lookup.
    file: Option<String>,
}

impl ComposeProject {
    /// A project whose Compose file is `compose.yaml` at its root. `yaml` is
    /// the file's content without its `name:` line.
    fn new(suffix: &str, yaml: &str) -> Self {
        Self::create(suffix, "compose.yaml", yaml, None)
    }

    /// A project whose only Compose file is `file`, relative to its root,
    /// which Compose's own lookup does not find.
    fn with_file(suffix: &str, file: &str, yaml: &str) -> Self {
        Self::create(suffix, file, yaml, Some(file.to_string()))
    }

    fn create(suffix: &str, path: &str, yaml: &str, file: Option<String>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("alba-it-{suffix}-{}", std::process::id());
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("name: {name}\n{yaml}")).unwrap();
        Self { dir, name, file }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The beam a test opens its first session for.
    fn beam(&self) -> String {
        self.name.clone()
    }

    /// Another beam of this project, for a test opening several sessions.
    fn other_beam(&self, which: &str) -> String {
        format!("{}-{which}", self.name)
    }

    /// Runs `docker compose <args>` in the project.
    fn compose(&self, args: &[&str]) -> std::process::Output {
        let mut command = std::process::Command::new("docker");
        command.current_dir(self.path()).arg("compose");
        if let Some(file) = &self.file {
            command.arg("-f").arg(self.path().join(file));
        }
        command.args(args).output().unwrap()
    }

    /// The id of `service`'s container, running or not.
    fn container_id(&self, service: &str) -> Option<String> {
        let output = self.compose(&["ps", "-a", "-q", service]);
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!id.is_empty()).then_some(id)
    }
}

impl Drop for ComposeProject {
    fn drop(&mut self) {
        let _ = self.compose(&["--profile", "*", "down", "-v", "--rmi", "local"]);
        let Ok(output) = std::process::Command::new("docker")
            .args([
                "ps",
                "-a",
                "--filter",
                "label=alba.beam",
                "--format",
                "{{.ID}} {{.Label \"alba.beam\"}}",
            ])
            .output()
        else {
            return;
        };
        let own = format!("{}-", self.name);
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Some((id, beam)) = line.split_once(' ') else {
                continue;
            };
            if beam == self.name || beam.starts_with(&own) {
                let _ = std::process::Command::new("docker")
                    .args(["rm", "-f", id])
                    .output();
            }
        }
    }
}

/// The ids of the containers labelled for `beam`; `all` includes stopped ones.
fn labelled_containers(beam: &str, all: bool) -> Vec<String> {
    let mut args = vec!["ps", "-q"];
    if all {
        args.push("-a");
    }
    let filter = format!("label=alba.beam={beam}");
    args.extend(["--filter", &filter]);
    let output = std::process::Command::new("docker")
        .args(&args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Waits until no container, running or not, is labelled for `beam`, failing
/// the test past `within`.
async fn assert_no_container_within(beam: &str, within: Duration) {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let left = labelled_containers(beam, true);
        if left.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "containers still labelled for `{beam}` after {within:?}: {left:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn options(service: &str) -> serde_json::Value {
    serde_json::json!({"service": service, "file": null})
}

fn beam_context(
    beam: &str,
    dir: &Path,
    options: serde_json::Value,
) -> (BeamContext, UnboundedReceiver<OutputLine>) {
    let (output, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        BeamContext {
            beam: beam.to_string(),
            dir: dir.to_path_buf(),
            options,
            output,
            cancel: Default::default(),
        },
        rx,
    )
}

fn exec_context() -> (ExecContext, UnboundedReceiver<OutputLine>) {
    let (output, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        ExecContext {
            output,
            cancel: Default::default(),
        },
        rx,
    )
}

fn spec(command: impl Into<String>, cwd: &Path) -> CommandSpec {
    CommandSpec {
        command: command.into(),
        env: vec![],
        cwd: cwd.to_path_buf(),
    }
}

async fn drain_text(rx: &mut UnboundedReceiver<OutputLine>) -> Vec<String> {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line.text);
    }
    lines
}

/// Cancels `cancel` after `delay`, returning the instant it did so.
fn cancel_after(
    cancel: tokio_util::sync::CancellationToken,
    delay: Duration,
) -> tokio::sync::oneshot::Receiver<tokio::time::Instant> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let now = tokio::time::Instant::now();
        cancel.cancel();
        let _ = tx.send(now);
    });
    rx
}

/// A service `tool` bind-mounting the project root on `/app`.
const TOOL: &str = "services:
  tool:
    image: alpine:3
    working_dir: /app
    volumes: [\".:/app\"]
";

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_command_runs_in_the_service_in_the_translated_directory() {
    let project = ComposeProject::new("translated-dir", TOOL);
    let beam_dir: PathBuf = project.path().join("modules").join("billing");
    std::fs::create_dir_all(&beam_dir).unwrap();

    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), &beam_dir, options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let mut command = spec("pwd && echo \"$GREETING\"", &beam_dir);
    command.env = vec![("GREETING".to_string(), "hej".to_string())];
    let result = session.execute(command, ctx).await.unwrap();

    assert_eq!(result.exit_code, 0);
    assert_eq!(
        drain_text(&mut rx).await,
        vec!["/app/modules/billing", "hej"]
    );
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn state_left_by_one_command_is_visible_to_the_next() {
    let project = ComposeProject::new("state", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, _rx) = exec_context();
    let touched = session
        .execute(spec("touch /tmp/marker", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(touched.exit_code, 0);
    let (ctx, _rx) = exec_context();
    let listed = session
        .execute(spec("ls /tmp/marker", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(
        listed.exit_code, 0,
        "the second command should see the file the first one wrote"
    );
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_exit_code_of_a_failing_command_is_reported() {
    let project = ComposeProject::new("exit-code", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, _rx) = exec_context();
    let result = session
        .execute(spec("exit 7", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 7);
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_service_behind_a_profile_is_reachable() {
    let project = ComposeProject::new(
        "profile",
        "services:
  tool:
    image: alpine:3
    profiles: [tools]
    volumes: [\".:/app\"]
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("echo ok", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(drain_text(&mut rx).await, vec!["ok"]);
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_declared_file_is_used_instead_of_the_lookup() {
    // Relative paths in a Compose file resolve against the file's own
    // directory: `..` is the project root.
    let project = ComposeProject::with_file(
        "declared-file",
        "ops/tools.yaml",
        "services:
  tool:
    image: alpine:3
    volumes: [\"..:/app\"]
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(
        &project.beam(),
        project.path(),
        serde_json::json!({"service": "tool", "file": "ops/tools.yaml"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("pwd", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(drain_text(&mut rx).await, vec!["/app"]);
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn two_sessions_on_one_service_run_at_the_same_time() {
    let project = ComposeProject::new("two-sessions", TOOL);
    let executor = ComposeExecutor::new();
    let (first_beam, second_beam) = (project.beam(), project.other_beam("second"));
    let (first_ctx, _first_rx) = beam_context(&first_beam, project.path(), options("tool"));
    let (second_ctx, _second_rx) = beam_context(&second_beam, project.path(), options("tool"));

    let (first, second) = tokio::join!(executor.open(first_ctx), executor.open(second_ctx));
    let (first, second) = (first.unwrap(), second.unwrap());

    assert_eq!(labelled_containers(&first_beam, false).len(), 1);
    assert_eq!(labelled_containers(&second_beam, false).len(), 1);
    first.close().await.unwrap();
    second.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_service_command_and_ports_are_not_used() {
    let project = ComposeProject::new(
        "command-ports",
        "services:
  tool:
    image: alpine:3
    command: sleep 3600
    ports: [\"38080:80\"]
    volumes: [\".:/app\"]
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let containers = labelled_containers(&project.beam(), false);
    assert_eq!(containers.len(), 1);
    let ports = std::process::Command::new("docker")
        .args(["port", &containers[0]])
        .output()
        .unwrap();
    assert!(
        ports.stdout.is_empty(),
        "expected no published port, found: {}",
        String::from_utf8_lossy(&ports.stdout)
    );

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("echo ok", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(drain_text(&mut rx).await, vec!["ok"]);
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_running_service_of_the_project_is_untouched() {
    let project = ComposeProject::new(
        "running-untouched",
        "services:
  web:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
",
    );
    assert!(project.compose(&["up", "-d", "web"]).status.success());
    let web = project.container_id("web").unwrap();

    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    session.close().await.unwrap();

    assert_eq!(project.container_id("web"), Some(web.clone()));
    let running = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", &web])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&running.stdout).trim(), "true");
}

/// A service `tool` built from the project root, whose Dockerfile is
/// `dockerfile`.
fn built_tool(suffix: &str, dockerfile: &str) -> ComposeProject {
    let project = ComposeProject::new(
        suffix,
        "services:
  tool:
    build: .
    volumes: [\".:/app\"]
",
    );
    std::fs::write(project.path().join("Dockerfile"), dockerfile).unwrap();
    project
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_image_is_built_when_the_service_declares_a_build() {
    // An image built from an older Dockerfile is already there: `run` alone
    // would start from it, so the session must build again.
    let project = built_tool("build", "FROM alpine:3\nRUN touch /stale\n");
    assert!(project.compose(&["build", "tool"]).status.success());
    std::fs::write(
        project.path().join("Dockerfile"),
        "FROM alpine:3\nRUN touch /built\n",
    )
    .unwrap();
    let executor = ComposeExecutor::new();
    let (beam_ctx, mut output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("test -f /built", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert!(drain_text(&mut rx).await.is_empty());
    session.close().await.unwrap();
    let beam_output = drain_text(&mut output_rx).await;
    assert!(
        beam_output.is_empty(),
        "expected nothing on the beam's output, found: {beam_output:?}"
    );
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_failing_build_puts_its_log_on_the_beams_output() {
    let project = built_tool(
        "failing-build",
        "FROM alpine:3\nRUN echo boom-marker && false\n",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, mut output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let error = executor.open(beam_ctx).await.err().unwrap();

    assert!(
        error
            .message
            .contains("docker compose build exited with code"),
        "unexpected message: {}",
        error.message
    );
    let beam_output = drain_text(&mut output_rx).await;
    assert!(
        beam_output.iter().any(|line| line.contains("boom-marker")),
        "expected the build log on the beam's output, found: {beam_output:?}"
    );
}

/// Options that evaluation would have refused can still reach `open` from a
/// caller that builds its own `BeamContext`: a `service` that reads as a flag
/// is refused before any `docker` command line is built, so this runs
/// without a daemon.
#[tokio::test]
async fn a_service_that_reads_as_a_flag_is_refused_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let executor = ComposeExecutor::new();
    let (beam_ctx, mut output_rx) = beam_context("flag-service", dir.path(), options("--help"));
    let error = executor.open(beam_ctx).await.err().unwrap();

    assert!(
        error.message.contains("invalid compose options")
            && error.message.contains("must not start with `-`"),
        "unexpected message: {}",
        error.message
    );
    assert!(
        drain_text(&mut output_rx).await.is_empty(),
        "no docker command may have run"
    );
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn an_unknown_service_fails_with_composes_message() {
    let project = ComposeProject::new("unknown-service", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, mut output_rx) = beam_context(&project.beam(), project.path(), options("nope"));
    let error = executor.open(beam_ctx).await.err().unwrap();

    assert!(
        error.message.contains("service `nope`"),
        "unexpected message: {}",
        error.message
    );
    let beam_output = drain_text(&mut output_rx).await;
    assert!(
        beam_output
            .iter()
            .any(|line| line.contains("no such service: nope")),
        "expected Compose's message on the beam's output, found: {beam_output:?}"
    );
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_directory_outside_every_bind_fails_the_command() {
    let project = ComposeProject::new("outside-bind", TOOL);
    let elsewhere = tempfile::tempdir().unwrap();
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, _rx) = exec_context();
    let error = session
        .execute(spec("echo never", elsewhere.path()), ctx)
        .await
        .unwrap_err();
    assert!(
        error.message.contains("no bind mount of service `tool`"),
        "unexpected message: {}",
        error.message
    );
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_service_without_a_shell_fails_and_leaves_no_container() {
    let project = ComposeProject::new(
        "no-shell",
        "services:
  hello:
    image: hello-world
    volumes: [\".:/app\"]
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("hello"));
    let error = executor.open(beam_ctx).await.err().unwrap();

    assert!(
        error.message.contains("service `hello`"),
        "unexpected message: {}",
        error.message
    );
    assert!(labelled_containers(&project.beam(), true).is_empty());
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn closing_removes_the_container() {
    let project = ComposeProject::new("close", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    assert_eq!(labelled_containers(&project.beam(), true).len(), 1);

    session.close().await.unwrap();
    assert!(labelled_containers(&project.beam(), true).is_empty());
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn cancelling_a_command_removes_the_container() {
    let project = ComposeProject::new("cancel-command", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (output, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ExecContext {
        output,
        cancel: cancel.clone(),
    };
    let cancelled_at = cancel_after(cancel, Duration::from_millis(500));
    session
        .execute(spec("sleep 60", project.path()), ctx)
        .await
        .unwrap();
    let elapsed = tokio::time::Instant::now() - cancelled_at.await.unwrap();

    // The stop grace (5 s) plus a margin.
    assert!(
        elapsed < Duration::from_secs(8),
        "execute() took {elapsed:?} to return after cancellation"
    );
    // Already gone when `execute` returns, before any `close`.
    assert!(labelled_containers(&project.beam(), true).is_empty());
    session.close().await.unwrap();
}

/// The stand-in for an Alba process killed outright: the session is dropped
/// without `close` or `kill`, so only its stdin pipe closing can remove the
/// container.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_container_whose_stdin_closes_is_removed() {
    let project = ComposeProject::new("stdin-closes", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    assert_eq!(labelled_containers(&project.beam(), true).len(), 1);

    drop(session);
    assert_no_container_within(&project.beam(), Duration::from_secs(10)).await;
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn cancelling_the_open_leaves_no_container() {
    let project = ComposeProject::new("cancel-open", TOOL);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let _cancelled_at = cancel_after(beam_ctx.cancel.clone(), Duration::from_millis(100));

    let error = executor.open(beam_ctx).await.err().unwrap();
    assert_eq!(error.message, "compose session start cancelled");
    assert!(labelled_containers(&project.beam(), true).is_empty());
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn cancelling_the_open_during_a_build_stops_it_promptly() {
    let project = built_tool("cancel-build", "");
    // The project name keeps the build cache from answering at once.
    std::fs::write(
        project.path().join("Dockerfile"),
        format!("FROM alpine:3\nRUN echo {} && sleep 20\n", project.name),
    )
    .unwrap();
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let cancelled_at = cancel_after(beam_ctx.cancel.clone(), Duration::from_secs(2));

    let error = executor.open(beam_ctx).await.err().unwrap();
    let elapsed = tokio::time::Instant::now() - cancelled_at.await.unwrap();

    assert_eq!(error.message, "compose session start cancelled");
    assert!(
        elapsed < Duration::from_secs(3),
        "open() took {elapsed:?} to return after cancellation"
    );
    assert!(labelled_containers(&project.beam(), true).is_empty());
}

/// A service `tool` depending on `db`, a long-running `alpine:3` quick to
/// stop.
const TOOL_WITH_DB: &str = "services:
  db:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on: [db]
";

/// Whether the container `id` is running.
fn is_running(id: &str) -> bool {
    let output = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", id])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim() == "true"
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn an_absent_dependency_is_started_kept_after_close_and_removed_at_shutdown() {
    let project = ComposeProject::new("dep-absent", TOOL_WITH_DB);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();

    let db = project
        .container_id("db")
        .expect("db should have been started");
    assert!(is_running(&db));
    session.close().await.unwrap();
    assert!(is_running(&db), "db should outlive the session");

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_stopped_dependency_is_started_then_stopped_again() {
    let project = ComposeProject::new("dep-stopped", TOOL_WITH_DB);
    assert!(project.compose(&["up", "-d", "db"]).status.success());
    assert!(project.compose(&["stop", "db"]).status.success());
    let db = project.container_id("db").unwrap();

    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    assert!(is_running(&db), "db should have been started");
    session.close().await.unwrap();

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), Some(db.clone()));
    assert!(!is_running(&db), "db should be stopped again");
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_running_dependency_is_left_alone() {
    let project = ComposeProject::new("dep-running", TOOL_WITH_DB);
    assert!(project.compose(&["up", "-d", "db"]).status.success());
    let db = project.container_id("db").unwrap();

    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    session.close().await.unwrap();
    executor.shutdown().await.unwrap();

    assert_eq!(project.container_id("db"), Some(db.clone()));
    assert!(is_running(&db), "db was not Alba's to stop");
}

/// The id of `service`'s own container, leaving out the one-off containers
/// `docker compose run` created for it.
fn service_container_id(project: &ComposeProject, service: &str) -> Option<String> {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=com.docker.compose.project={}", project.name),
            "--filter",
            &format!("label=com.docker.compose.service={service}"),
            "--filter",
            "label=com.docker.compose.oneoff=False",
        ])
        .output()
        .unwrap();
    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!id.is_empty()).then_some(id)
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_one_off_container_does_not_make_a_dependency_running() {
    let project = ComposeProject::new("dep-one-off", TOOL_WITH_DB);
    // A `docker compose run` of the dependency, labelled so that the
    // project's teardown removes it.
    let one_off = project.compose(&[
        "run",
        "-d",
        "--no-deps",
        "-l",
        &format!("alba.beam={}", project.beam()),
        "db",
        "sleep",
        "3600",
    ]);
    assert!(one_off.status.success());
    let one_off = String::from_utf8_lossy(&one_off.stdout).trim().to_string();
    assert_eq!(service_container_id(&project, "db"), None, "precondition");

    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    let db = service_container_id(&project, "db").expect("db should have been started");
    assert!(is_running(&db));
    session.close().await.unwrap();
    executor.shutdown().await.unwrap();

    assert_eq!(
        service_container_id(&project, "db"),
        None,
        "the db container Alba started should be removed"
    );
    assert!(is_running(&one_off), "the one-off was not Alba's to stop");
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_running_dependency_whose_definition_changed_is_not_recreated() {
    let project = ComposeProject::new("dep-diverged", TOOL_WITH_DB);
    assert!(project.compose(&["up", "-d", "db"]).status.success());
    let db = project.container_id("db").unwrap();
    std::fs::write(
        project.path().join("compose.yaml"),
        format!(
            "name: {}\n{}",
            project.name,
            TOOL_WITH_DB.replace(
                "    command: sleep 3600\n",
                "    command: sleep 3600\n    environment: [DIVERGED=1]\n"
            )
        ),
    )
    .unwrap();

    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();

    assert_eq!(project.container_id("db"), Some(db.clone()));
    assert!(is_running(&db));
    session.close().await.unwrap();
    executor.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_second_session_reuses_the_dependency_the_first_started() {
    let project = ComposeProject::new("dep-reused", TOOL_WITH_DB);
    let executor = ComposeExecutor::new();

    let (first_ctx, _first_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let first = executor.open(first_ctx).await.unwrap();
    let db = project.container_id("db").unwrap();
    first.close().await.unwrap();

    let (second_ctx, _second_rx) = beam_context(
        &project.other_beam("second"),
        project.path(),
        options("tool"),
    );
    let second = executor.open(second_ctx).await.unwrap();
    assert_eq!(project.container_id("db"), Some(db.clone()));
    assert!(is_running(&db));
    second.close().await.unwrap();

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn sessions_opening_together_start_a_dependency_once() {
    let project = ComposeProject::new("dep-together", TOOL_WITH_DB);
    let executor = ComposeExecutor::new();
    let (first_ctx, _first_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let (second_ctx, _second_rx) = beam_context(
        &project.other_beam("second"),
        project.path(),
        options("tool"),
    );

    let (first, second) = tokio::join!(executor.open(first_ctx), executor.open(second_ctx));
    let (first, second) = (first.unwrap(), second.unwrap());

    let containers = project.compose(&["ps", "-a", "-q", "db"]);
    assert_eq!(
        String::from_utf8_lossy(&containers.stdout).lines().count(),
        1
    );
    first.close().await.unwrap();
    second.close().await.unwrap();
    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_healthcheck_is_waited_for() {
    let project = ComposeProject::new(
        "dep-healthy",
        "services:
  db:
    image: alpine:3
    command: [\"sh\", \"-c\", \"sleep 2 && touch /tmp/ready && sleep 3600\"]
    stop_grace_period: 1s
    healthcheck:
      test: [\"CMD\", \"test\", \"-f\", \"/tmp/ready\"]
      interval: 500ms
      retries: 20
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on:
      db:
        condition: service_healthy
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    // Before the session's first command.
    let db = project.container_id("db").unwrap();
    let health = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Health.Status}}", &db])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&health.stdout).trim(), "healthy");
    let (ctx, _rx) = exec_context();
    let result = session
        .execute(spec("true", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    session.close().await.unwrap();
    executor.shutdown().await.unwrap();
}

/// Opens a session of `tool` on `project` and returns what its first
/// command, `command`, printed, with its exit code.
async fn first_command(
    project: &ComposeProject,
    executor: &ComposeExecutor,
    command: &str,
) -> (i32, Vec<String>) {
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();
    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec(command, project.path()), ctx)
        .await
        .unwrap();
    session.close().await.unwrap();
    (result.exit_code, drain_text(&mut rx).await)
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_one_shot_dependency_runs_to_completion() {
    let project = ComposeProject::new(
        "dep-one-shot",
        "services:
  migrate:
    image: alpine:3
    command: [\"sh\", \"-c\", \"sleep 2 && touch /data/migrated\"]
    volumes: [\"data:/data\"]
  tool:
    image: alpine:3
    volumes: [\".:/app\", \"data:/data\"]
    depends_on:
      migrate:
        condition: service_completed_successfully
volumes:
  data:
",
    );
    let executor = ComposeExecutor::new();

    let (exit_code, _) = first_command(&project, &executor, "test -f /data/migrated").await;
    assert_eq!(
        exit_code, 0,
        "migrate should have completed before the command"
    );
    let migrate = project.container_id("migrate").unwrap();
    let exited = std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.Status}} {{.State.ExitCode}}",
            &migrate,
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&exited.stdout).trim(), "exited 0");

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("migrate"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_one_shot_behind_another_dependency_runs_to_completion() {
    let project = ComposeProject::new(
        "dep-one-shot-transitive",
        "services:
  migrate:
    image: alpine:3
    command: [\"sh\", \"-c\", \"sleep 2 && touch /data/migrated\"]
    volumes: [\"data:/data\"]
  db:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
    depends_on:
      migrate:
        condition: service_completed_successfully
  tool:
    image: alpine:3
    volumes: [\".:/app\", \"data:/data\"]
    depends_on: [db]
volumes:
  data:
",
    );
    let executor = ComposeExecutor::new();

    let (exit_code, _) = first_command(&project, &executor, "test -f /data/migrated").await;
    assert_eq!(
        exit_code, 0,
        "migrate should have completed before the command"
    );

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
    assert_eq!(project.container_id("migrate"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_one_shot_reached_through_another_dependency_runs_once() {
    // `tool` and `db` both wait for `migrate`: starting `db` runs it, and
    // starting it again would run it a second time.
    let project = ComposeProject::new(
        "dep-one-shot-once",
        "services:
  migrate:
    image: alpine:3
    command: [\"sh\", \"-c\", \"echo run >> /data/runs\"]
    volumes: [\"data:/data\"]
  db:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
    depends_on:
      migrate:
        condition: service_completed_successfully
  tool:
    image: alpine:3
    volumes: [\".:/app\", \"data:/data\"]
    depends_on:
      db:
        condition: service_started
      migrate:
        condition: service_completed_successfully
volumes:
  data:
",
    );
    let executor = ComposeExecutor::new();

    let (exit_code, output) = first_command(&project, &executor, "wc -l < /data/runs").await;
    assert_eq!(exit_code, 0);
    assert_eq!(output, vec!["1"]);
    executor.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_one_shot_that_already_ran_is_run_again_and_left_exited() {
    let project = ComposeProject::new(
        "dep-one-shot-ran",
        "services:
  migrate:
    image: alpine:3
    command: [\"sh\", \"-c\", \"sleep 1 && echo run >> /data/runs\"]
    volumes: [\"data:/data\"]
  tool:
    image: alpine:3
    volumes: [\".:/app\", \"data:/data\"]
    depends_on:
      migrate:
        condition: service_completed_successfully
volumes:
  data:
",
    );
    assert!(project.compose(&["up", "-d", "migrate"]).status.success());
    assert!(project.compose(&["wait", "migrate"]).status.success());
    let migrate = project.container_id("migrate").unwrap();
    let executor = ComposeExecutor::new();

    // Run again by `up`, as `docker compose run` would, and awaited.
    let (exit_code, output) = first_command(&project, &executor, "wc -l < /data/runs").await;
    assert_eq!(exit_code, 0);
    assert_eq!(output, vec!["2"]);

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("migrate"), Some(migrate.clone()));
    let state = std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.Status}} {{.State.ExitCode}}",
            &migrate,
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&state.stdout).trim(), "exited 0");
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_one_shot_dependency_that_fails_fails_the_beam() {
    let project = ComposeProject::new(
        "dep-one-shot-fails",
        "services:
  migrate:
    image: alpine:3
    command: [\"sh\", \"-c\", \"exit 3\"]
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on:
      migrate:
        condition: service_completed_successfully
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let error = executor.open(beam_ctx).await.err().unwrap();

    assert_eq!(
        error.message,
        "docker compose wait exited with code 3 for the dependency `migrate` of service `tool`"
    );
    assert!(labelled_containers(&project.beam(), true).is_empty());
    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("migrate"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn cancelling_the_open_while_a_dependency_starts_puts_it_back() {
    let project = ComposeProject::new(
        "dep-cancel-start",
        "services:
  db:
    image: alpine:3
    command: [\"sh\", \"-c\", \"sleep 30 && touch /tmp/ready && sleep 3600\"]
    stop_grace_period: 1s
    healthcheck:
      test: [\"CMD\", \"test\", \"-f\", \"/tmp/ready\"]
      interval: 500ms
      retries: 100
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on:
      db:
        condition: service_healthy
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let _cancelled_at = cancel_after(beam_ctx.cancel.clone(), Duration::from_secs(3));

    let error = executor.open(beam_ctx).await.err().unwrap();
    assert_eq!(error.message, "compose session start cancelled");
    assert!(
        project.container_id("db").is_some(),
        "db should have been created"
    );

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_dependency_in_the_short_syntax_that_exits_at_once_lets_the_beam_open() {
    let project = ComposeProject::new(
        "dep-started-exits",
        "services:
  init:
    image: alpine:3
    command: \"true\"
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on: [init]
",
    );
    let executor = ComposeExecutor::new();

    let (exit_code, _) = first_command(&project, &executor, "true").await;

    assert_eq!(exit_code, 0);
    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("init"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_started_dependency_is_not_waited_for_its_health() {
    // `web` never becomes healthy: only `service_healthy` would wait for it.
    let project = ComposeProject::new(
        "dep-started-unhealthy",
        "services:
  web:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
    healthcheck:
      test: [\"CMD\", \"false\"]
      interval: 500ms
      retries: 2
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on:
      web:
        condition: service_started
",
    );
    let executor = ComposeExecutor::new();

    let (exit_code, _) = first_command(&project, &executor, "true").await;

    assert_eq!(exit_code, 0);
    let web = project.container_id("web").unwrap();
    assert!(is_running(&web));
    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("web"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_dependency_that_cannot_start_fails_the_beam_and_the_rest_is_put_back() {
    let project = ComposeProject::new(
        "dep-ghost",
        "services:
  db:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
  cache:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
  ghost:
    image: alba-it-no-such-image:0
    pull_policy: never
    depends_on: [cache]
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on: [db, ghost]
",
    );
    // `db` starts beside `ghost`; `cache` is surely started before `ghost`
    // fails, being its dependency.
    let executor = ComposeExecutor::new();
    let (beam_ctx, mut output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let error = executor.open(beam_ctx).await.err().unwrap();

    assert_eq!(
        error.message,
        "docker compose up exited with code 1 for the dependencies of service `tool`"
    );
    let beam_output = drain_text(&mut output_rx).await;
    assert!(
        beam_output
            .iter()
            .any(|line| line.contains("alba-it-no-such-image")),
        "expected Compose's message on the beam's output, found: {beam_output:?}"
    );
    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
    assert_eq!(project.container_id("cache"), None);
    assert_eq!(project.container_id("ghost"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn an_optional_dependency_that_cannot_start_is_skipped_with_a_warning() {
    let project = ComposeProject::new(
        "dep-optional",
        "services:
  db:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
  ghost:
    image: alba-it-no-such-image:0
    pull_policy: never
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on:
      db:
        condition: service_started
      ghost:
        condition: service_started
        required: false
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, mut output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();

    let warnings: Vec<_> = drain_text(&mut output_rx)
        .await
        .into_iter()
        .filter(|line| line.starts_with("optional dependency"))
        .collect();
    assert_eq!(
        warnings,
        vec![
            "optional dependency `ghost` of service `tool` did not start (docker compose up \
             exited with code 1): continuing without it"
        ]
    );
    let db = project.container_id("db").unwrap();
    assert!(is_running(&db));
    let (ctx, _rx) = exec_context();
    let result = session
        .execute(spec("true", project.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    session.close().await.unwrap();

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
    assert_eq!(project.container_id("ghost"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn cancelling_a_command_keeps_the_dependencies_until_shutdown() {
    let project = ComposeProject::new("dep-cancel", TOOL_WITH_DB);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let mut session = executor.open(beam_ctx).await.unwrap();
    let db = project.container_id("db").unwrap();

    let (output, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ExecContext {
        output,
        cancel: cancel.clone(),
    };
    let _cancelled_at = cancel_after(cancel, Duration::from_millis(500));
    session
        .execute(spec("sleep 60", project.path()), ctx)
        .await
        .unwrap();
    session.close().await.unwrap();
    assert!(is_running(&db), "db should outlive the cancelled session");

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
}

/// Removes a Docker volume when dropped: an anonymous volume outlives its
/// container and no project teardown reaches it.
struct VolumeCleanup(String);

impl Drop for VolumeCleanup {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", "-f", &self.0])
            .output();
    }
}

fn volume_exists(name: &str) -> bool {
    let volumes = std::process::Command::new("docker")
        .args(["volume", "ls", "-q"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&volumes.stdout)
        .lines()
        .any(|line| line == name)
}

/// A dependency whose healthcheck passes only after 30 s, keeping the
/// session start of `tool` waiting.
const TOOL_WITH_SLOW_DB: &str = "services:
  db:
    image: alpine:3
    command: [\"sh\", \"-c\", \"sleep 30 && touch /tmp/ready && sleep 3600\"]
    stop_grace_period: 1s
    healthcheck:
      test: [\"CMD\", \"test\", \"-f\", \"/tmp/ready\"]
      interval: 500ms
      retries: 100
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on:
      db:
        condition: service_healthy
";

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn dropping_the_open_while_a_dependency_starts_puts_it_back() {
    let project = ComposeProject::new("dep-drop-start", TOOL_WITH_SLOW_DB);
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));

    let dropped = tokio::time::timeout(Duration::from_secs(3), executor.open(beam_ctx)).await;
    assert!(dropped.is_err(), "the open should still be waiting for db");
    assert!(
        project.container_id("db").is_some(),
        "db should have been created"
    );

    executor.shutdown().await.unwrap();
    assert_eq!(project.container_id("db"), None);
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn shutdown_never_removes_a_volume() {
    // A named volume, and an anonymous one as an image's `VOLUME` creates:
    // `rm -v` would remove the anonymous one.
    let project = ComposeProject::new(
        "dep-volume",
        "services:
  db:
    image: alpine:3
    command: sleep 3600
    stop_grace_period: 1s
    volumes: [\"data:/data\", \"/scratch\"]
  tool:
    image: alpine:3
    volumes: [\".:/app\"]
    depends_on: [db]
volumes:
  data:
",
    );
    let executor = ComposeExecutor::new();
    let (beam_ctx, _output_rx) = beam_context(&project.beam(), project.path(), options("tool"));
    let session = executor.open(beam_ctx).await.unwrap();
    let db = project.container_id("db").unwrap();
    let anonymous = std::process::Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .Mounts}}{{if eq .Destination \"/scratch\"}}{{.Name}}{{end}}{{end}}",
            &db,
        ])
        .output()
        .unwrap();
    let anonymous = VolumeCleanup(
        String::from_utf8_lossy(&anonymous.stdout)
            .trim()
            .to_string(),
    );
    assert!(!anonymous.0.is_empty());
    session.close().await.unwrap();
    executor.shutdown().await.unwrap();

    assert_eq!(project.container_id("db"), None);
    let named = format!("{}_data", project.name);
    assert!(
        volume_exists(&named),
        "the volume `{named}` should still exist"
    );
    assert!(
        volume_exists(&anonymous.0),
        "the anonymous volume `{}` should still exist",
        anonymous.0
    );
}

#[tokio::test]
async fn shutdown_with_nothing_started_succeeds() {
    ComposeExecutor::new().shutdown().await.unwrap();
}
