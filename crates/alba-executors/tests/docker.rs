//! Integration tests for [`alba_executors::DockerExecutor`] against a real
//! docker daemon. Every test is `#[ignore]`d (see the reason string on each)
//! because they require `docker` on the `PATH` and a running daemon; run
//! them explicitly with `cargo test -p alba-executors --test docker --
//! --ignored`. All of them use `alpine:3` and label their container with
//! `alba.beam=<a test-unique beam name>` so a stray container left behind
//! by a failing test is easy to find (`docker ps -a --filter
//! label=alba.beam=<name>`) and never collides with another test's
//! container.

use std::path::Path;

use alba_executors::{BeamContext, CommandSpec, DockerExecutor, ExecContext, Executor, OutputLine};
use tokio::sync::mpsc::UnboundedReceiver;

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

#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn runs_a_command_in_the_declared_image_and_reports_its_exit_code() {
    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam, mut output_rx) = beam_context(
        "docker-it-runs-a-command",
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam).await.unwrap();

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
    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam, _output_rx) = beam_context(
        "docker-it-state-persists",
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam).await.unwrap();

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
    let project_root = tempfile::tempdir().unwrap();
    std::fs::write(project_root.path().join("hello.txt"), "hi from the host").unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam, _output_rx) = beam_context(
        "docker-it-project-mount",
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let mut session = executor.open(beam).await.unwrap();

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
    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());
    let beam_name = "docker-it-close-removes";

    let (beam, _output_rx) = beam_context(
        beam_name,
        project_root.path(),
        serde_json::json!({"image": "alpine:3"}),
    );
    let session = executor.open(beam).await.unwrap();
    session.close().await.unwrap();

    let output = tokio::process::Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=alba.beam={beam_name}"),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.stdout.is_empty(),
        "expected no container left behind, found: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}
