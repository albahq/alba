//! [`ComposeExecutor`]: runs a beam's commands inside a container of one
//! service of a Docker Compose project, through the `docker compose` CLI.
//!
//! A session reads the service from `docker compose config`, builds its image
//! when the service declares a build, then starts one container of it with
//! `docker compose run --rm -T --no-deps ... <service> sh`, `sh` reading from
//! a stdin pipe Alba holds for the whole session. Each [`CommandSpec`] then
//! runs through `docker exec` in that container, in the directory the
//! service's bind mounts map the command's own directory to (see
//! [`ServiceDescription::container_cwd`]).
//!
//! The held pipe is what reclaims the container whatever becomes of Alba:
//! when it closes (the session closes, or the Alba process dies), `sh` reads
//! the end of its input and exits, and `--rm` removes the container. So the
//! `run` process is deliberately not killed when the session is dropped.
//!
//! The container keeps the service's own user, environment and mounts:
//! nothing that overrides them is passed. Its `command` is replaced by that
//! `sh`, and its `ports` are not published, as `docker compose run` does by
//! default.
//!
//! The service's dependencies are not left to `docker compose run`, which
//! would recreate one whose definition changed under a developer's running
//! stack: `run` gets `--no-deps`, and the session start brings them up itself
//! with `docker compose up -d --no-recreate`, honouring their `depends_on`
//! conditions (see [`service::DependencyStart`]). The executor records the
//! ones it started and the state each was in before, absent or stopped, and
//! puts them back in that state at [`Executor::shutdown`]: a dependency that
//! was already running is never recorded, so never stopped. The containers it
//! started outlive the session that started them, for the next session of the
//! run to reuse.

mod dependencies;
mod service;
mod started;

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::container::{self, DRAIN_PERIOD, STOP_GRACE, container_name, run_quiet, spawn_error};
use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor,
    OutputLine, Stream,
};

use dependencies::Dependencies;
use service::ServiceDescription;

/// What the compose executor needs from the host, finishing the message of a
/// failed `docker` spawn.
const COMPOSE_REQUIREMENT: &str =
    "the compose executor requires Docker with the Compose plugin on the PATH";

/// How long a killed `docker compose run` leaves the daemon to finish a
/// container creation it had already asked for, before the second removal
/// that catches it: see `ComposeSession::abandon`.
const CREATION_SETTLE: Duration = Duration::from_secs(1);

/// How long a session start waits for its container to accept commands before
/// one line on the beam's output names the prerequisites a container stuck
/// there usually misses. The wait itself goes on: an image pull inside
/// `docker compose run` can legitimately take minutes.
const READY_NOTICE_AFTER: Duration = Duration::from_secs(30);

/// Runs beam commands inside a container of a Compose service. See the module
/// doc comment for the container lifecycle.
#[derive(Debug)]
pub struct ComposeExecutor {
    /// How long a session start waits for its container to accept commands
    /// before telling the beam what it may be missing: [`READY_NOTICE_AFTER`]
    /// outside tests.
    ready_notice_after: Duration,
    /// The service dependencies this executor started, to put back at
    /// shutdown.
    dependencies: Dependencies,
}

impl ComposeExecutor {
    pub fn new() -> Self {
        Self {
            ready_notice_after: READY_NOTICE_AFTER,
            dependencies: Dependencies::default(),
        }
    }
}

impl Default for ComposeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// The executor options a beam declares.
#[derive(Debug, serde::Deserialize)]
struct ComposeOptions {
    /// The service the beam's commands run in.
    service: String,
    /// The Compose file, relative to the beam's directory, when Compose's
    /// own lookup from that directory is not the one wanted.
    #[serde(default)]
    file: Option<String>,
}

impl ComposeOptions {
    fn from_value(options: &serde_json::Value) -> Result<Self, ExecError> {
        let invalid = |cause: String| ExecError {
            message: format!("invalid compose options: {cause}"),
        };
        let options: Self =
            serde_json::from_value(options.clone()).map_err(|error| invalid(error.to_string()))?;
        if options.service.is_empty() {
            return Err(invalid("`service` is empty".to_string()));
        }
        Ok(options)
    }
}

/// How to address the beam's Compose project: every `docker compose`
/// invocation runs in the beam's directory, with the declared file if any.
#[derive(Debug)]
struct Project {
    dir: PathBuf,
    file: Option<PathBuf>,
}

impl Project {
    fn new(beam: &BeamContext, options: &ComposeOptions) -> Self {
        Self {
            dir: beam.dir.clone(),
            file: options.file.as_ref().map(|file| beam.dir.join(file)),
        }
    }

    /// `docker compose <args>` for this project, quiet about its progress,
    /// with every stream null until the caller pipes the ones it reads.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("docker");
        command.current_dir(&self.dir).arg("compose");
        if let Some(file) = &self.file {
            command.arg("-f").arg(file);
        }
        command
            .args(["--progress", "quiet"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
}

#[async_trait::async_trait]
impl Executor for ComposeExecutor {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError> {
        let options = ComposeOptions::from_value(&beam.options)?;
        let project = Project::new(&beam, &options);
        let service = options.service.as_str();

        let config = run_step(
            project.command(&["config", "--format", "json", service]),
            &beam,
        )
        .await?;
        if !config.status.success() {
            return Err(step_failed("config", config.status, service));
        }
        let description = ServiceDescription::parse(&step_stdout(config, "config")?, service)?;

        if description.has_build {
            // Quiet, a successful build prints nothing worth keeping: its
            // stdout, or a failure to read it, does not decide the outcome.
            let build = run_step(project.command(&["build", service]), &beam).await?;
            if !build.status.success() {
                return Err(step_failed("build", build.status, service));
            }
        }

        // Before the service's own container: `run` is given `--no-deps` so
        // that it never starts or recreates them itself.
        if !description.dependencies.is_empty() {
            self.dependencies
                .start(&project, &description, &beam)
                .await?;
        }

        let session =
            start_container(&project, description, &beam, self.ready_notice_after).await?;
        Ok(Box::new(session))
    }

    /// Puts every dependency this executor started back in the state it
    /// had: removed (`rm -s -f`, never its volumes) when it had no
    /// container, stopped when it was stopped. Every project is attempted
    /// and its record emptied, whatever the outcome; the failures are
    /// reported together.
    ///
    /// No time bound is added: Compose bounds each `rm -s -f` and `stop` by
    /// the services' own `stop_grace_period`. A project whose start was
    /// interrupted is released a second time once `CREATION_SETTLE` has
    /// passed, for a creation or a start that `up` had in flight.
    async fn shutdown(&self) -> Result<(), ExecError> {
        self.dependencies.release().await
    }
}

/// What one `docker compose` step of a session start left behind.
struct StepOutcome {
    status: ExitStatus,
    /// The step's whole stdout, or why it could not be read in full.
    stdout: Result<String, String>,
}

/// Runs one `docker compose` step of a session start to completion, its
/// stdout captured and its stderr relayed to the beam's output, so that
/// Compose's own diagnostic reaches the user when the step fails. Cancelling
/// the beam kills the step.
async fn run_step(mut command: Command, beam: &BeamContext) -> Result<StepOutcome, ExecError> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| spawn_error(error, COMPOSE_REQUIREMENT))?;

    let mut stdout = child
        .stdout
        .take()
        .expect("child spawned with piped stdout");
    let stderr = child
        .stderr
        .take()
        .expect("child spawned with piped stderr");
    let mut stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut buf)
            .await
            .map(|_| buf)
            .map_err(|error| error.to_string())
    });
    let mut stderr_task = tokio::spawn(stream_lines(stderr, Stream::Stderr, beam.output.clone()));

    let status = tokio::select! {
        result = child.wait() => result.map_err(|error| ExecError {
            message: format!("failed to wait for `docker compose`: {error}"),
        }),
        _ = beam.cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(start_cancelled())
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(error);
        }
    };

    // The step has exited; a descendant still holding one of its pipes
    // must not hold the beam, so the readers are bounded like the ones of
    // `docker exec`.
    let drained = async {
        let (stdout, _) = tokio::join!(&mut stdout_task, &mut stderr_task);
        stdout.unwrap_or_else(|error| Err(format!("the reader stopped: {error}")))
    };
    let stdout = match tokio::time::timeout(DRAIN_PERIOD, drained).await {
        Ok(stdout) => stdout,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            Err(format!(
                "it was still open {} s after the command exited",
                DRAIN_PERIOD.as_secs()
            ))
        }
    };
    Ok(StepOutcome { status, stdout })
}

/// The whole stdout of a step whose outcome depends on it.
fn step_stdout(outcome: StepOutcome, subcommand: &str) -> Result<String, ExecError> {
    outcome.stdout.map_err(|cause| ExecError {
        message: format!("could not read the output of `docker compose {subcommand}`: {cause}"),
    })
}

/// Starts the beam's container with `docker compose run` and waits until its
/// `sh` reads the pipe Alba holds: see [`wait_until_ready`].
async fn start_container(
    project: &Project,
    description: ServiceDescription,
    beam: &BeamContext,
    notice_after: Duration,
) -> Result<ComposeSession, ExecError> {
    let name = container_name(&beam.beam);
    let label = format!("alba.beam={}", beam.beam);
    let service = description.service.as_str();
    let mut command = project.command(&[
        "run",
        "--rm",
        "-T",
        "--no-deps",
        "--name",
        &name,
        "--label",
        &label,
        service,
        "sh",
    ]);
    // No `kill_on_drop`: the session is reclaimed by its stdin pipe closing,
    // not by killing this process (see the module doc comment).
    let mut run = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_error(error, COMPOSE_REQUIREMENT))?;

    let stdin = run.stdin.take().expect("child spawned with piped stdin");
    let stdout = run.stdout.take().expect("child spawned with piped stdout");
    let stderr = run.stderr.take().expect("child spawned with piped stderr");

    let token = format!("alba-ready-{name}");
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let stdout_task = tokio::spawn(relay_stdout(
        stdout,
        token.clone(),
        ready_tx,
        beam.output.clone(),
    ));
    let stderr_task = tokio::spawn(stream_lines(stderr, Stream::Stderr, beam.output.clone()));

    let mut session = ComposeSession {
        name,
        description,
        run,
        stdin: Some(stdin),
        relays: vec![stdout_task, stderr_task],
    };
    match wait_until_ready(&mut session, &token, ready_rx, beam, notice_after).await {
        Ok(()) => Ok(session),
        Err(error) => {
            session.abandon().await;
            Err(error)
        }
    }
}

/// Waits until the container accepts commands: the `sh` it runs is asked to
/// echo `token`, which it can only do once the container is up and reading
/// the pipe. Fails as soon as the `run` process exits, and when the beam is
/// cancelled. Past `notice_after`, one line on the beam's output names the
/// prerequisites a container that never answers usually misses; the wait
/// goes on.
async fn wait_until_ready(
    session: &mut ComposeSession,
    token: &str,
    ready: tokio::sync::oneshot::Receiver<()>,
    beam: &BeamContext,
    notice_after: Duration,
) -> Result<(), ExecError> {
    let stdin = session
        .stdin
        .as_mut()
        .expect("the stdin pipe is held until the session closes");
    let asked = async {
        // A failed write or a stdout closed before the token both mean the
        // `run` process is going away: its exit, awaited below, is the
        // diagnostic worth reporting.
        if stdin
            .write_all(format!("echo {token}\n").as_bytes())
            .await
            .is_err()
            || stdin.flush().await.is_err()
        {
            return false;
        }
        ready.await.is_ok()
    };

    let service = session.description.service.clone();
    let run = &mut session.run;
    let notice = tokio::time::sleep(notice_after);
    tokio::pin!(asked, notice);
    let mut noticed = false;
    loop {
        tokio::select! {
            ready = &mut asked => {
                if ready {
                    return Ok(());
                }
                return tokio::select! {
                    exited = tokio::time::timeout(STOP_GRACE, run.wait()) => match exited {
                        Ok(Ok(status)) => Err(step_failed("run", status, &service)),
                        Ok(Err(error)) => Err(run_wait_failed(error)),
                        Err(_) => Err(ExecError {
                            message: format!(
                                "the container of service `{service}` closed its output \
                                 without becoming ready"
                            ),
                        }),
                    },
                    _ = beam.cancel.cancelled() => Err(start_cancelled()),
                };
            }
            status = run.wait() => {
                return match status {
                    Ok(status) => Err(step_failed("run", status, &service)),
                    Err(error) => Err(run_wait_failed(error)),
                };
            }
            _ = beam.cancel.cancelled() => return Err(start_cancelled()),
            _ = &mut notice, if !noticed => {
                noticed = true;
                let _ = beam.output.send(OutputLine {
                    stream: Stream::Stderr,
                    text: format!(
                        "still waiting for the container of service `{service}` to accept \
                         commands: its image must provide `sh`, and its entrypoint, if it \
                         has one, must execute the command it is given"
                    ),
                });
            }
        }
    }
}

fn run_wait_failed(error: std::io::Error) -> ExecError {
    ExecError {
        message: format!("failed to wait for `docker compose run`: {error}"),
    }
}

/// Relays the container's stdout to the beam's output line by line, except
/// the first line equal to `token`: that one is the readiness answer (see
/// [`wait_until_ready`]), signalled on `ready` instead.
async fn relay_stdout(
    stdout: tokio::process::ChildStdout,
    token: String,
    ready: tokio::sync::oneshot::Sender<()>,
    output: UnboundedSender<OutputLine>,
) {
    let (lines_tx, mut lines_rx) = tokio::sync::mpsc::unbounded_channel::<OutputLine>();
    let forward = async move {
        let mut ready = Some(ready);
        while let Some(line) = lines_rx.recv().await {
            if ready.is_some() && line.text == token {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(());
                }
                continue;
            }
            let _ = output.send(line);
        }
    };
    tokio::join!(stream_lines(stdout, Stream::Stdout, lines_tx), forward);
}

/// A non-zero exit of `docker compose <subcommand>` while starting a session.
/// Compose's own message is already on the beam's output.
fn step_failed(subcommand: &str, status: ExitStatus, service: &str) -> ExecError {
    let code = status.code().unwrap_or(-1);
    ExecError {
        message: format!(
            "docker compose {subcommand} exited with code {code} for service `{service}`"
        ),
    }
}

fn start_cancelled() -> ExecError {
    ExecError {
        message: "compose session start cancelled".into(),
    }
}

/// The session opened for one beam: the container `name` that
/// `docker compose run` started, kept alive by the `stdin` pipe its `sh`
/// reads.
struct ComposeSession {
    name: String,
    description: ServiceDescription,
    run: Child,
    /// Held until the session ends: dropping it ends the container's `sh`.
    stdin: Option<ChildStdin>,
    /// The tasks relaying the `run` process's stdout and stderr.
    relays: Vec<JoinHandle<()>>,
}

impl ComposeSession {
    /// Gives up on a session that never became ready: kills the `run`
    /// process unless it already exited, and removes the container, the
    /// removal bounded by `STOP_GRACE`.
    ///
    /// A killed `run` may have asked the daemon to create the container
    /// moments before: a creation the daemon already accepted can land after
    /// a first removal and leave a `created` container behind. So a killed
    /// `run` gets a second removal once [`CREATION_SETTLE`] has passed. A
    /// `run` that exited on its own has nothing left in flight.
    async fn abandon(mut self) {
        self.stdin = None;
        let exited = matches!(self.run.try_wait(), Ok(Some(_)));
        if !exited {
            let _ = self.run.start_kill();
            let _ = self.run.wait().await;
        }
        let name = self.name.clone();
        let removal = async move {
            run_quiet(&["rm", "-f", &name]).await;
            if !exited {
                tokio::time::sleep(CREATION_SETTLE).await;
                run_quiet(&["rm", "-f", &name]).await;
            }
        };
        let _ = tokio::time::timeout(STOP_GRACE, removal).await;
        self.drain_relays().await;
    }

    /// Lets the relays deliver what the `run` process printed last, bounded
    /// by `DRAIN_PERIOD` against a descendant holding a pipe open.
    async fn drain_relays(&mut self) {
        let relays = std::mem::take(&mut self.relays);
        let aborts: Vec<_> = relays.iter().map(JoinHandle::abort_handle).collect();
        let drained = async {
            for relay in relays {
                let _ = relay.await;
            }
        };
        if tokio::time::timeout(DRAIN_PERIOD, drained).await.is_err() {
            for abort in aborts {
                abort.abort();
            }
        }
    }
}

#[async_trait::async_trait]
impl ExecSession for ComposeSession {
    async fn execute(
        &mut self,
        cmd: CommandSpec,
        ctx: ExecContext,
    ) -> Result<ExecResult, ExecError> {
        let cwd = self.description.container_cwd(&cmd.cwd)?;
        container::exec(&self.name, &cwd, &cmd, ctx, COMPOSE_REQUIREMENT).await
    }

    async fn close(mut self: Box<Self>) -> Result<(), ExecError> {
        // Closing the pipe ends `sh`, and `--rm` removes the container.
        self.stdin = None;
        if tokio::time::timeout(STOP_GRACE, self.run.wait())
            .await
            .is_err()
        {
            let _ = self.run.start_kill();
            let _ = self.run.wait().await;
        }
        self.drain_relays().await;
        // Unconditional: the container is gone already unless the `run`
        // process had to be killed.
        container::remove(&self.name, COMPOSE_REQUIREMENT).await
    }

    async fn kill(self: Box<Self>) {
        container::force_remove(&self.name).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removes the throwaway project and every container labelled for its
    /// beam, even when the test panics.
    struct Teardown {
        dir: PathBuf,
        beam: String,
    }

    impl Drop for Teardown {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .current_dir(&self.dir)
                .args(["compose", "down", "-v"])
                .output();
            let Ok(output) = std::process::Command::new("docker")
                .args([
                    "ps",
                    "-aq",
                    "--filter",
                    &format!("label=alba.beam={}", self.beam),
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

    /// The notice delay is the seam: the real one is 30 s.
    #[tokio::test]
    #[ignore = "requires a running docker daemon"]
    async fn a_container_slow_to_accept_commands_gets_a_notice_naming_the_prerequisite() {
        let dir = tempfile::tempdir().unwrap();
        let beam = format!("alba-it-slow-ready-{}", std::process::id());
        // The entrypoint ignores the `sh` it is given: nothing reads the pipe.
        std::fs::write(
            dir.path().join("compose.yaml"),
            format!(
                "name: {beam}\nservices:\n  tool:\n    image: alpine:3\n    \
                 entrypoint: [\"sh\", \"-c\", \"sleep 3600\"]\n    volumes: [\".:/app\"]\n"
            ),
        )
        .unwrap();
        let _teardown = Teardown {
            dir: dir.path().to_path_buf(),
            beam: beam.clone(),
        };

        let executor = ComposeExecutor {
            ready_notice_after: Duration::from_millis(500),
            ..ComposeExecutor::new()
        };
        let (output, mut output_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let context = BeamContext {
            beam: beam.clone(),
            dir: dir.path().to_path_buf(),
            options: serde_json::json!({"service": "tool", "file": null}),
            output,
            cancel: cancel.clone(),
        };
        let canceller = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            cancel.cancel();
        });

        let error = executor.open(context).await.err().unwrap();
        canceller.await.unwrap();
        assert_eq!(error.message, "compose session start cancelled");

        let mut lines = Vec::new();
        while let Ok(line) = output_rx.try_recv() {
            lines.push(line.text);
        }
        let notices: Vec<_> = lines
            .iter()
            .filter(|line| line.starts_with("still waiting"))
            .collect();
        assert_eq!(
            notices,
            vec![
                "still waiting for the container of service `tool` to accept commands: its \
                 image must provide `sh`, and its entrypoint, if it has one, must execute \
                 the command it is given"
            ],
            "unexpected output: {lines:?}"
        );
    }
}
