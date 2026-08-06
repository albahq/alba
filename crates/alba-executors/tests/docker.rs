//! Integration tests for [`alba_executors::DockerExecutor`] against a real
//! docker daemon. Every test is `#[ignore]`d (see the reason string on each)
//! because they require `docker` on the `PATH` and a running daemon; run
//! them explicitly with `cargo test -p alba-executors --test docker --
//! --ignored`. All of them use `alpine:3` and label their container with
//! `alba.beam=<a test-and-run-unique beam name>` (the pid suffix from
//! [`beam_name`] makes it unique across runs, not just within one) so a
//! stray container left behind by a failing test is easy to find (`docker
//! ps -a --filter label=alba.beam=<name>`), never collides with another
//! test's container, and never collides with a previous run's leftover on
//! a shared runner.
//!
//! Every test holds a [`ContainerGuard`] for the whole test body. `Drop`
//! runs even when a test panics mid-assertion, so a failing test cannot
//! leave a live container behind: the session's container is a dormant
//! `sleep` that never stops on its own, so `--rm` alone (which only
//! reclaims a container once it *stops*) is not a safety net here.

use std::path::Path;
use std::time::Duration;

use alba_executors::{BeamContext, CommandSpec, DockerExecutor, ExecContext, Executor, OutputLine};
use tokio::sync::mpsc::UnboundedReceiver;

/// A beam name unique to both this test (`suffix`) and this run (the
/// process id): the label it produces (`alba.beam=<name>`, see
/// `run_args`) must not collide with a leftover from an earlier run on a
/// shared runner, or `close_removes_the_container` could see someone
/// else's container and fail forever.
fn beam_name(suffix: &str) -> String {
    format!("docker-it-{suffix}-{}", std::process::id())
}

/// Unconditional cleanup for one test's container, found by its
/// `alba.beam` label rather than the (session-private) generated
/// container name. `Drop` runs during a panicking unwind, so holding one
/// of these for the duration of a test guarantees no live container
/// survives a failed assertion, regardless of where it fails.
struct ContainerGuard {
    label: String,
}

impl ContainerGuard {
    fn new(beam: &str) -> Self {
        Self {
            label: beam.to_string(),
        }
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let Ok(output) = std::process::Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=alba.beam={}", self.label),
            ])
            .output()
        else {
            return;
        };
        for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", id])
                .output();
        }
    }
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

/// Cancels `cancel` after `delay`, returning the instant it did so — mirrors
/// `shell.rs`'s test helper of the same name, used to measure
/// cancellation-to-return latency without including command spawn time.
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

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn runs_a_command_in_the_declared_image_and_reports_its_exit_code() {
    let beam = beam_name("runs-a-command");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, mut output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("echo hello from docker", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let lines = drain_text(&mut rx).await;
    assert!(
        lines.contains(&"hello from docker".to_string()),
        "expected the echoed line in {lines:?}"
    );

    let (ctx, _rx) = exec_context();
    let result = session
        .execute(spec("exit 7", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 7);

    session.close().await.unwrap();
    let _ = drain_text(&mut output_rx).await;
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn container_state_persists_across_commands_of_one_session() {
    let beam = beam_name("state-persists");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, _rx) = exec_context();
    let result = session
        .execute(spec("echo state > /tmp/probe", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("cat /tmp/probe", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let lines = drain_text(&mut rx).await;
    assert_eq!(
        lines,
        vec!["state".to_string()],
        "the second command should see the file the first one wrote, proving \
         both ran in the same container"
    );

    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_project_mount_makes_host_files_visible() {
    let beam = beam_name("project-mount");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    std::fs::write(project_root.path().join("hello.txt"), "hi from the host").unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("cat hello.txt", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let lines = drain_text(&mut rx).await;
    assert_eq!(lines, vec!["hi from the host".to_string()]);

    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn close_removes_the_container() {
    let beam = beam_name("close-removes");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let session = executor.open(beam_ctx).await.unwrap();
    session.close().await.unwrap();

    let output = tokio::process::Command::new("docker")
        .args(["ps", "-aq", "--filter", &format!("label=alba.beam={beam}")])
        .output()
        .await
        .unwrap();
    assert!(
        output.stdout.is_empty(),
        "expected no container left behind, found: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Pins the bounded-teardown contract `execute`'s cancellation path
/// documents: stopping and removing the container, then reaping the
/// local `docker exec` process, must together stay within `STOP_GRACE`
/// (5s) rather than being able to run each unbounded step to completion
/// (which could total far more than 5s against a slow daemon). `sleep 60`
/// dies immediately once `docker stop` delivers its signal, so a working
/// bounded teardown returns in a couple of seconds; a regression back to
/// the unbounded shape would still return well under 60s but would no
/// longer respect the advertised grace.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn cancelling_execute_returns_within_the_stop_grace() {
    let beam = beam_name("cancel-execute");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (output, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = ExecContext {
        output,
        cancel: cancel.clone(),
    };
    let cancelled_at = cancel_after(cancel, Duration::from_millis(300));

    session
        .execute(spec("sleep 60", project_root.path()), ctx)
        .await
        .unwrap();
    let returned_at = tokio::time::Instant::now();

    let elapsed_since_cancel = returned_at - cancelled_at.await.unwrap();
    assert!(
        elapsed_since_cancel < Duration::from_secs(8),
        "execute() took {elapsed_since_cancel:?} to return after cancellation fired; \
         the teardown sequence (docker stop, rm, wait) must be bounded by the 5s \
         STOP_GRACE as a whole, not free to run each step to completion unbounded"
    );

    // The teardown already stopped and removed the container; `close`
    // must still succeed (it treats "already gone" as success) rather
    // than surfacing a "No such container" failure for what is actually
    // cancellation's expected outcome.
    session.close().await.unwrap();
}
