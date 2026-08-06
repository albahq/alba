//! [`PluginExecutor`]: runs a beam's commands through an external
//! `alba-executor-<name>` binary, speaking the wire protocol defined in
//! [`crate::protocol`] over its stdin/stdout.
//!
//! One plugin process is spawned per beam session (mirroring
//! [`crate::DockerExecutor`]'s one-container-per-session model) and kept
//! alive across every command the session runs: the handshake happens
//! once in `open`, and the same process answers every subsequent
//! `execute`/`close` message. Stderr is free-form diagnostic output the
//! plugin can produce at any point across the whole session, so it is
//! relayed by a task spawned once in `open` and never joined — its
//! lifetime is the process's, not any single command's.
//!
//! This module only spawns and speaks to a plugin binary whose path is
//! already known; resolving `alba-executor-<name>` on the `PATH` and
//! dispatching to it is a later concern, not this one's.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::protocol::{HostMessage, PROTOCOL_VERSION, PluginMessage, WireStream};
use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor,
    OutputLine, Stream,
};

/// How long `open` waits for the plugin to answer the handshake (`ready`
/// or `error`) before giving up and killing it.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long, after asking a plugin to cancel the command it is running,
/// `execute` waits for it to actually stop (any trailing output plus a
/// final `exit`/`error`) before giving up and killing the process.
const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// Spawns `binary` (an `alba-executor-<name>` plugin) and speaks
/// [`crate::protocol`] to it over stdin/stdout for the lifetime of one
/// beam session. See the module doc comment for the process lifecycle.
pub struct PluginExecutor {
    binary: PathBuf,
    args: Vec<String>,
    handshake_timeout: Duration,
    grace: Duration,
}

impl PluginExecutor {
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            args: Vec::new(),
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            grace: DEFAULT_GRACE,
        }
    }

    /// Same as [`Self::new`] with explicit timeouts, for tests that need
    /// the handshake and cancellation grace to expire quickly rather than
    /// waiting out the real ten-second/five-second defaults.
    pub fn with_timeouts(binary: PathBuf, handshake: Duration, grace: Duration) -> Self {
        Self {
            binary,
            args: Vec::new(),
            handshake_timeout: handshake,
            grace,
        }
    }

    /// Extra argv for the spawned plugin. Alba itself never sets any; the
    /// test suite uses it to select a scripted fake plugin's behavior.
    ///
    /// That selector cannot travel over the wire protocol: a misbehaving
    /// fake (one that stays silent, or answers garbage, or exits
    /// immediately) does so *before* it would ever see an `open` message,
    /// so there is no protocol message left to carry a mode selector by
    /// the time the fake would need to read one. It cannot travel through
    /// an environment variable either — the test suite runs many such
    /// scenarios concurrently in one process, and `std::env` is global
    /// mutable state shared by every test, so setting it from one test
    /// races every other test doing the same. Argv is neither: it is
    /// fixed at spawn time, private to the spawned process, and available
    /// before the child has read a single byte — the only channel left
    /// that is both pre-handshake and free of shared mutable state.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }
}

#[async_trait::async_trait]
impl Executor for PluginExecutor {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError> {
        let mut child = Command::new(&self.binary)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| ExecError {
                message: format!(
                    "cannot spawn plugin `{}`: {error} — is it on the PATH?",
                    self.binary.display()
                ),
            })?;

        let stdin = child.stdin.take().expect("child spawned with piped stdin");
        let stdout = child
            .stdout
            .take()
            .expect("child spawned with piped stdout");
        let stderr = child
            .stderr
            .take()
            .expect("child spawned with piped stderr");

        // Detached: stderr is free-form output the plugin can produce at
        // any point across the whole session, not just during the
        // command executing right now, so unlike a single command's
        // stdout/stderr (see docker.rs, shell.rs) this task is never
        // joined or bounded by a single `execute` call — it simply runs
        // until the plugin's stderr pipe closes, i.e. until the process
        // exits.
        tokio::spawn(stream_lines(stderr, Stream::Stderr, beam.output.clone()));

        let mut session = PluginSession {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            grace: self.grace,
        };

        // A write failure here is ignored rather than surfaced directly:
        // a plugin that dies immediately (the `die` scripted mode, or a
        // real plugin crashing on startup) can race this write against
        // its own exit and close its stdin first, which would otherwise
        // surface as a raw broken-pipe error instead of the plugin's
        // actual exit code. Falling through to the read below reports
        // that death uniformly, through the same EOF handling
        // `read_message` already gives every other premature exit.
        let _ = session
            .send(&HostMessage::Open {
                protocol: PROTOCOL_VERSION,
                beam: beam.beam,
                dir: beam.dir.display().to_string(),
                options: beam.options,
            })
            .await;

        match tokio::time::timeout(self.handshake_timeout, session.read_message()).await {
            Ok(Ok(PluginMessage::Ready)) => Ok(Box::new(session)),
            Ok(Ok(PluginMessage::Error { message })) => {
                kill(&mut session.child).await;
                Err(ExecError { message })
            }
            Ok(Ok(other)) => {
                kill(&mut session.child).await;
                Err(ExecError {
                    message: format!(
                        "plugin sent an unexpected message during the handshake: {other:?}"
                    ),
                })
            }
            Ok(Err(error)) => {
                kill(&mut session.child).await;
                Err(error)
            }
            Err(_elapsed) => {
                kill(&mut session.child).await;
                Err(ExecError {
                    message: format!(
                        "plugin did not answer the handshake within {}s",
                        self.handshake_timeout.as_secs()
                    ),
                })
            }
        }
    }
}

/// The session opened for one beam: the plugin process, kept alive and
/// spoken to for every command the beam runs.
struct PluginSession {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    grace: Duration,
}

impl PluginSession {
    /// Serializes and writes one [`HostMessage`] as a single line.
    async fn send(&mut self, message: &HostMessage) -> Result<(), ExecError> {
        let mut line = serde_json::to_string(message).expect("HostMessage always serializes");
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|error| ExecError {
                message: format!("failed to write to plugin: {error}"),
            })
    }

    /// Reads and decodes one [`PluginMessage`] line. Shared by the
    /// handshake (`open`) and the command loop (`execute`): an
    /// undecodable line quotes the offending text, and EOF (the plugin's
    /// stdout closed, i.e. it exited) is resolved to its exit code rather
    /// than a bare "connection closed" error.
    async fn read_message(&mut self) -> Result<PluginMessage, ExecError> {
        match self.stdout.next_line().await {
            Ok(Some(line)) => serde_json::from_str(&line).map_err(|_error| ExecError {
                message: format!("plugin spoke a non-protocol line: `{line}`"),
            }),
            Ok(None) => {
                let code = self
                    .child
                    .wait()
                    .await
                    .ok()
                    .and_then(|status| status.code())
                    .unwrap_or(-1);
                Err(ExecError {
                    message: format!("plugin exited with code {code}"),
                })
            }
            Err(error) => Err(ExecError {
                message: format!("failed to read from plugin: {error}"),
            }),
        }
    }
}

#[async_trait::async_trait]
impl ExecSession for PluginSession {
    async fn execute(
        &mut self,
        cmd: CommandSpec,
        ctx: ExecContext,
    ) -> Result<ExecResult, ExecError> {
        self.send(&HostMessage::Execute {
            command: cmd.command,
            env: cmd.env,
            cwd: cmd.cwd.display().to_string(),
        })
        .await?;

        // `None` until cancellation, then the single deadline the rest of
        // this command's teardown is bounded by: asking the plugin to
        // stop and reading whatever it says next (more output, then a
        // final `exit`/`error`) all share this one overall grace period
        // rather than each read getting its own — a plugin that keeps
        // talking without ever actually finishing must not be able to
        // hold `execute` past `grace` by doing so.
        let mut deadline: Option<tokio::time::Instant> = None;

        loop {
            let message = match deadline {
                None => tokio::select! {
                    message = self.read_message() => message,
                    _ = ctx.cancel.cancelled() => {
                        let _ = self.send(&HostMessage::Cancel).await;
                        deadline = Some(tokio::time::Instant::now() + self.grace);
                        continue;
                    }
                },
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, self.read_message()).await {
                        Ok(message) => message,
                        Err(_elapsed) => {
                            let _ = self.child.start_kill();
                            let _ = self.child.wait().await;
                            return Ok(ExecResult { exit_code: -1 });
                        }
                    }
                }
            };

            match message? {
                PluginMessage::Output { stream, text } => {
                    let stream = match stream {
                        WireStream::Stdout => Stream::Stdout,
                        WireStream::Stderr => Stream::Stderr,
                    };
                    // If nobody is listening anymore, keep going anyway —
                    // the plugin still needs its exit/error read off the
                    // pipe so the process does not sit half-drained.
                    let _ = ctx.output.send(OutputLine { stream, text });
                }
                PluginMessage::Exit { code } => return Ok(ExecResult { exit_code: code }),
                PluginMessage::Error { message } => return Err(ExecError { message }),
                PluginMessage::Ready => {
                    return Err(ExecError {
                        message: "plugin sent an unexpected `ready` message during execute"
                            .to_string(),
                    });
                }
            }
        }
    }

    async fn close(mut self: Box<Self>) -> Result<(), ExecError> {
        // Best-effort: a plugin that already died has nothing left to
        // read this, and the wait below reports that death either way.
        let _ = self.send(&HostMessage::Close).await;

        match tokio::time::timeout(self.grace, self.child.wait()).await {
            Ok(Ok(_status)) => Ok(()),
            Ok(Err(error)) => Err(ExecError {
                message: format!("failed to wait for plugin: {error}"),
            }),
            Err(_elapsed) => {
                let _ = self.child.start_kill();
                self.child
                    .wait()
                    .await
                    .map(|_status| ())
                    .map_err(|error| ExecError {
                        message: format!("failed to wait for plugin after kill: {error}"),
                    })
            }
        }
    }
}

/// Kills and reaps a child, ignoring errors from both: used on every
/// handshake failure path so the plugin process never outlives `open`,
/// regardless of whether it is still running (a misbehaving-but-alive
/// plugin) or already gone (nothing to kill, the wait just observes the
/// exit that already happened).
async fn kill(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}
