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
