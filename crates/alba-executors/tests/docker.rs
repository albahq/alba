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

/// A declared `user` is who the beam's commands run as, whatever user the
/// image itself defaults to (`alpine:3` defaults to root).
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn commands_run_as_the_declared_user() {
    let beam = beam_name("declared-user");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3", "user": "1234:5678"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("echo $(id -u):$(id -g)", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let lines = drain_text(&mut rx).await;
    assert_eq!(lines, ["1234:5678"]);

    session.close().await.unwrap();
}

/// `user "host"` runs the commands as whoever runs Alba, so a file a beam
/// writes into the project mount belongs to that person and not to root.
#[cfg(unix)]
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_host_user_runs_the_commands_when_asked() {
    let beam = beam_name("host-user");
    let _guard = ContainerGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"image": "alpine:3", "user": "host"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("echo $(id -u):$(id -g)", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    let lines = drain_text(&mut rx).await;
    assert_eq!(lines, [host_user()]);

    session.close().await.unwrap();
}

/// The `uid:gid` of the process running the tests, read from the `id`
/// command rather than from the code under test.
#[cfg(unix)]
fn host_user() -> String {
    let id = |flag: &str| {
        let output = std::process::Command::new("id").arg(flag).output().unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    format!("{}:{}", id("-u"), id("-g"))
}

/// Unconditional cleanup for the images one test builds, found by the
/// `alba.test` label its Dockerfile carries (see [`build_context`]).
struct ImageGuard {
    label: String,
}

impl ImageGuard {
    fn new(beam: &str) -> Self {
        Self {
            label: beam.to_string(),
        }
    }
}

impl Drop for ImageGuard {
    fn drop(&mut self) {
        let Ok(output) = std::process::Command::new("docker")
            .args([
                "images",
                "-q",
                "--filter",
                &format!("label=alba.test={}", self.label),
            ])
            .output()
        else {
            return;
        };
        for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            let _ = std::process::Command::new("docker")
                .args(["rmi", "-f", id])
                .output();
        }
    }
}

/// Writes a build context under `project_root/docker`: an image that
/// differs from `alpine:3` by one file, `/baked.txt`, so a test can tell
/// its commands ran in the image built from this context.
fn build_context(project_root: &Path, beam: &str) {
    let context = project_root.join("docker");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        format!("FROM alpine:3\nLABEL alba.test={beam}\nRUN echo baked > /baked.txt\n"),
    )
    .unwrap();
}

/// A beam that declares a build context runs in the image built from it,
/// with no image to name or to build beforehand.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn commands_run_in_the_image_built_from_the_declared_context() {
    let beam = beam_name("build-context");
    let _guard = ContainerGuard::new(&beam);
    let _images = ImageGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    build_context(project_root.path(), &beam);
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"build": "docker"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("cat /baked.txt", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(drain_text(&mut rx).await, ["baked"]);

    session.close().await.unwrap();
}

/// With both a build context and an image, the built image carries that
/// name, so it can be reused outside Alba.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_built_image_is_tagged_with_the_declared_image_name() {
    let beam = beam_name("build-tag");
    let _guard = ContainerGuard::new(&beam);
    let _images = ImageGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    build_context(project_root.path(), &beam);
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"build": "docker", "image": beam}),
    );
    let session = executor.open(beam_ctx).await.unwrap();
    session.close().await.unwrap();

    let tagged = std::process::Command::new("docker")
        .args(["run", "--rm", &beam, "cat", "/baked.txt"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&tagged.stdout).trim(), "baked");
}

/// A context docker cannot build fails the session with the reason docker
/// gave, rather than a later, unrelated `docker run` error.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_context_that_does_not_build_fails_the_session_with_dockers_reason() {
    let beam = beam_name("build-fails");
    let _guard = ContainerGuard::new(&beam);
    let _images = ImageGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    let context = project_root.path().join("docker");
    std::fs::create_dir(&context).unwrap();
    std::fs::write(
        context.join("Dockerfile"),
        format!("FROM alpine:3\nLABEL alba.test={beam}\nRUN exit 3\n"),
    )
    .unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, mut output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"build": "docker"}),
    );
    let error = match executor.open(beam_ctx).await {
        Ok(_) => panic!("a context that does not build must not open a session"),
        Err(error) => error,
    };

    assert!(
        error.message.contains("docker build exited with code"),
        "unexpected message: {}",
        error.message
    );
    let lines = drain_text(&mut output_rx).await.join("\n");
    assert!(
        lines.contains("exit 3"),
        "docker's own build error must reach the beam's output: {lines}"
    );
}

/// Unconditional cleanup for the named volume one test creates.
struct VolumeGuard {
    name: String,
}

impl Drop for VolumeGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["volume", "rm", "-f", &self.name])
            .output();
    }
}

/// A volume whose container path starts with `./` is mounted under the
/// beam's own directory inside the container: what a command writes there
/// lands in the volume, which outlives the container, and never in the
/// project directory on the host.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn a_relative_volume_is_mounted_under_the_beams_directory() {
    let beam = beam_name("relative-volume");
    let _guard = ContainerGuard::new(&beam);
    let _volume = VolumeGuard { name: beam.clone() };

    let project_root = tempfile::tempdir().unwrap();
    let beam_dir = project_root.path().join("app");
    std::fs::create_dir(&beam_dir).unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());
    let options = serde_json::json!({
        "image": "alpine:3",
        "volumes": [format!("{beam}:./deps")],
    });

    let (beam_ctx, _output_rx) = beam_context(&beam, &beam_dir, options.clone());
    let mut session = executor.open(beam_ctx).await.unwrap();
    let (ctx, _rx) = exec_context();
    let result = session
        .execute(spec("echo kept > deps/marker", &beam_dir), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    session.close().await.unwrap();

    let (beam_ctx, _output_rx) = beam_context(&beam, &beam_dir, options);
    let mut session = executor.open(beam_ctx).await.unwrap();
    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("cat deps/marker", &beam_dir), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(drain_text(&mut rx).await, ["kept"]);
    session.close().await.unwrap();

    assert!(
        !beam_dir.join("deps/marker").exists(),
        "the file must live in the volume, not in the project directory"
    );
}

/// A declared `dockerfile` is the file the image is built from, found
/// inside the build context: a context with no file named `Dockerfile` at
/// all still builds.
#[tokio::test]
#[ignore = "requires a running docker daemon"]
async fn the_image_is_built_from_the_declared_dockerfile() {
    let beam = beam_name("dockerfile");
    let _guard = ContainerGuard::new(&beam);
    let _images = ImageGuard::new(&beam);

    let project_root = tempfile::tempdir().unwrap();
    build_context(project_root.path(), &beam);
    let context = project_root.path().join("docker");
    std::fs::create_dir(context.join("images")).unwrap();
    std::fs::rename(
        context.join("Dockerfile"),
        context.join("images/dev.Dockerfile"),
    )
    .unwrap();
    let executor = DockerExecutor::new(project_root.path().to_path_buf());

    let (beam_ctx, _output_rx) = beam_context(
        &beam,
        project_root.path(),
        serde_json::json!({"build": "docker", "dockerfile": "images/dev.Dockerfile"}),
    );
    let mut session = executor.open(beam_ctx).await.unwrap();

    let (ctx, mut rx) = exec_context();
    let result = session
        .execute(spec("cat /baked.txt", project_root.path()), ctx)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(drain_text(&mut rx).await, ["baked"]);

    session.close().await.unwrap();
}
