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

mod service;

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};

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

use service::ServiceDescription;

/// What the compose executor needs from the host, finishing the message of a
/// failed `docker` spawn.
const COMPOSE_REQUIREMENT: &str =
    "the compose executor requires Docker with the Compose plugin on the PATH";

/// Runs beam commands inside a container of a Compose service. See the module
/// doc comment for the container lifecycle.
#[derive(Debug, Default)]
pub struct ComposeExecutor;

impl ComposeExecutor {
    pub fn new() -> Self {
        Self
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
        let description = ServiceDescription::parse(&config.stdout, service)?;

        if description.has_build {
            let build = run_step(project.command(&["build", service]), &beam).await?;
            if !build.status.success() {
                return Err(step_failed("build", build.status, service));
            }
        }

        // The service's dependencies are started here, before its own
        // container: `run` is given `--no-deps` so that it never starts or
        // recreates them itself.

        let session = start_container(&project, description, &beam).await?;
        Ok(Box::new(session))
    }
}

/// What one `docker compose` step of a session start left behind.
struct StepOutcome {
    status: ExitStatus,
    stdout: String,
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
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut buf).await;
        buf
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
        stdout.unwrap_or_default()
    };
    let stdout = match tokio::time::timeout(DRAIN_PERIOD, drained).await {
        Ok(stdout) => stdout,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            String::new()
        }
    };
    Ok(StepOutcome { status, stdout })
}

/// Starts the beam's container with `docker compose run` and waits until its
/// `sh` reads the pipe Alba holds: see [`wait_until_ready`].
async fn start_container(
    project: &Project,
    description: ServiceDescription,
    beam: &BeamContext,
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
    match wait_until_ready(&mut session, &token, ready_rx, beam).await {
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
/// cancelled.
async fn wait_until_ready(
    session: &mut ComposeSession,
    token: &str,
    ready: tokio::sync::oneshot::Receiver<()>,
    beam: &BeamContext,
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
    tokio::select! {
        ready = asked => {
            if ready {
                return Ok(());
            }
            match tokio::time::timeout(STOP_GRACE, run.wait()).await {
                Ok(Ok(status)) => Err(step_failed("run", status, &service)),
                Ok(Err(error)) => Err(ExecError {
                    message: format!("failed to wait for `docker compose run`: {error}"),
                }),
                Err(_) => Err(ExecError {
                    message: format!(
                        "the container of service `{service}` closed its output without \
                         becoming ready"
                    ),
                }),
            }
        }
        status = run.wait() => match status {
            Ok(status) => Err(step_failed("run", status, &service)),
            Err(error) => Err(ExecError {
                message: format!("failed to wait for `docker compose run`: {error}"),
            }),
        },
        _ = beam.cancel.cancelled() => Err(start_cancelled()),
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
    /// process and removes the container, bounded by `STOP_GRACE`, since the
    /// daemon may still be creating it.
    async fn abandon(mut self) {
        self.stdin = None;
        let _ = self.run.start_kill();
        let _ = self.run.wait().await;
        let _ = tokio::time::timeout(STOP_GRACE, run_quiet(&["rm", "-f", &self.name])).await;
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
