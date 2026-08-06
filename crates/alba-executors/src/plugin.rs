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
//! On unix the plugin is spawned as the leader of its own process group
//! (mirroring `shell.rs`'s `build_command`): a plugin's entire job is
//! running the beam's commands, so a forceful kill needs to reach what
//! it spawned to do that, not just the plugin binary itself. No windows
//! equivalent is wired up here, for the same reasons `shell.rs`'s
//! `terminate` doc comment gives.
//!
//! Every write to the plugin's stdin is bounded by whichever timeout
//! governs the phase it happens in (`handshake_timeout` for `open`,
//! `grace` for `execute`'s cancellation teardown and for `close`) — not
//! just the read that follows it. A plugin that stops draining its
//! stdin would otherwise be able to wedge the host once a write exceeds
//! the OS pipe buffer, regardless of how tightly the read side is
//! bounded.
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
#[cfg(unix)]
use crate::shell::kill_process_group;
use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor,
    OutputLine, Stream,
};

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
    /// How long [`Self::new`] waits for the plugin to answer the handshake
    /// (`ready` or `error`) before giving up and killing it. Bounds the
    /// `open` write as well as the read that follows it — see the module
    /// doc comment. Public so a caller that needs to reason about the
    /// exact timeout a plain [`Self::new`] session runs under — `alba
    /// plugin check`, in `alba-cli`, is the motivating one — can read it
    /// rather than guess a value that could silently drift from this one.
    pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

    /// How long, after asking a plugin to cancel the command it is
    /// running, [`ExecSession::execute`] waits for it to actually stop
    /// (any trailing output plus a final `exit`/`error`) before giving up
    /// and killing the process. Also bounds the initial `execute` write
    /// and the `close` write/wait — see the module doc comment. Public
    /// for the same reason as [`Self::DEFAULT_HANDSHAKE_TIMEOUT`].
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            args: Vec::new(),
            handshake_timeout: Self::DEFAULT_HANDSHAKE_TIMEOUT,
            grace: Self::DEFAULT_GRACE,
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
        // Destructured up front so `cancel` can be selected on below
        // while `beam_name`/`dir`/`options` are moved into the `open`
        // message independently — disjoint fields of a local binding,
        // not the whole struct.
        let BeamContext {
            beam: beam_name,
            dir,
            options,
            output,
            cancel,
        } = beam;

        let mut command = Command::new(&self.binary);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A dropped `Child` handle does not, by itself, stop the
            // process it represents — tokio only best-effort-reaps it
            // once it exits on its own. Every path that gives up on a
            // session already `kill`s and `wait`s explicitly (see
            // `kill` below); this is the backstop for the path that
            // doesn't: a caller dropping the `PluginSession` outright
            // (a panic, an `execute` error the engine does not recover
            // from, a task holding the session getting cancelled)
            // instead of awaiting `close`.
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn().map_err(|error| ExecError {
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
        tokio::spawn(stream_lines(stderr, Stream::Stderr, output.clone()));

        let mut session = PluginSession {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            grace: self.grace,
        };

        // The `open` write and the handshake read it waits for are
        // bounded together by `handshake_timeout` (see the module doc
        // comment), and the whole thing can additionally be interrupted
        // by the caller cancelling — mirroring `docker.rs`'s `open`,
        // which selects on cancellation during container start.
        let handshake = tokio::select! {
            result = tokio::time::timeout(self.handshake_timeout, async {
                // A write failure here is ignored rather than surfaced
                // directly: a plugin that dies immediately (the `die`
                // scripted mode, or a real plugin crashing on startup)
                // can race this write against its own exit and close
                // its stdin first, which would otherwise surface as a
                // raw broken-pipe error instead of the plugin's actual
                // exit code. Falling through to the read reports that
                // death uniformly, through the same EOF handling
                // `read_message` already gives every other premature
                // exit.
                let _ = session
                    .send(&HostMessage::Open {
                        protocol: PROTOCOL_VERSION,
                        beam: beam_name,
                        dir: dir.display().to_string(),
                        options,
                    })
                    .await;
                session.read_message().await
            }) => result,
            _ = cancel.cancelled() => {
                kill(&mut session.child).await;
                return Err(ExecError {
                    message: "plugin handshake cancelled".to_string(),
                });
            }
        };

        match handshake {
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
                // `read_message`'s own EOF message is phase-agnostic (it
                // is shared with `execute`, where "before answering the
                // handshake" would be false); the handshake-specific
                // context is added here, at the one call site that knows
                // it applies.
                let message = if error.message.starts_with("plugin exited with code") {
                    format!("{} before answering the handshake", error.message)
                } else {
                    error.message
                };
                Err(ExecError { message })
            }
            Err(_elapsed) => {
                kill(&mut session.child).await;
                Err(ExecError {
                    message: format!(
                        "plugin did not answer the handshake within {:?}",
                        self.handshake_timeout
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
    /// Callers are responsible for bounding this — see the module doc
    /// comment — `send` itself never times out.
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
        // Bounded by `grace`, the same budget a stalled cancellation
        // teardown gets below: a plugin that never drains stdin must not
        // be able to block this call before it has even received the
        // command to run.
        match tokio::time::timeout(
            self.grace,
            self.send(&HostMessage::Execute {
                command: cmd.command,
                env: cmd.env,
                cwd: cmd.cwd.display().to_string(),
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_elapsed) => {
                kill(&mut self.child).await;
                return Err(ExecError {
                    message: format!("plugin did not accept the command within {:?}", self.grace),
                });
            }
        }

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
                        let deadline_at = tokio::time::Instant::now() + self.grace;
                        // Bounded by the same deadline as everything
                        // else in this teardown sequence: a plugin that
                        // stops draining stdin must not be able to block
                        // this send forever either.
                        let _ = tokio::time::timeout_at(
                            deadline_at,
                            self.send(&HostMessage::Cancel),
                        )
                        .await;
                        deadline = Some(deadline_at);
                        continue;
                    }
                },
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, self.read_message()).await {
                        Ok(message) => message,
                        Err(_elapsed) => {
                            kill(&mut self.child).await;
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
        // The `close` write, shutting down our write half, and the wait
        // for the plugin to exit are all bounded together by `grace`
        // (see the module doc comment) — not just the wait, as before.
        let deadline = tokio::time::Instant::now() + self.grace;
        let outcome = tokio::time::timeout_at(deadline, async {
            // Best-effort: a plugin that already died has nothing left
            // to read this, and the wait below reports that death
            // either way.
            let _ = self.send(&HostMessage::Close).await;
            // Lets an EOF-driven plugin (one that exits on reaching the
            // end of stdin rather than parsing `close`'s content) act on
            // it immediately, instead of sitting until the deadline
            // forces a kill.
            let _ = self.stdin.shutdown().await;
            self.child.wait().await
        })
        .await;

        match outcome {
            Ok(Ok(_status)) => Ok(()),
            Ok(Err(error)) => Err(ExecError {
                message: format!("failed to wait for plugin: {error}"),
            }),
            Err(_elapsed) => {
                kill(&mut self.child).await;
                Ok(())
            }
        }
    }

    async fn kill(mut self: Box<Self>) {
        // The same group-aware, kill-and-reap teardown every other
        // give-up path in this file already uses (a failed handshake, an
        // expired cancellation grace, a `close` that timed out) — the
        // whole reason `kill` exists on the trait at all is that a bare
        // `Drop` here would reach only this process, not the process
        // group it leads (see `open`'s `process_group(0)`).
        kill(&mut self.child).await;
    }
}

/// Kills and reaps a child, ignoring errors from both: used on every
/// teardown path (a failed handshake, a cancellation grace that expired,
/// a close that timed out) so the plugin process — and everything it
/// spawned — never outlives the call that gave up on it.
///
/// On unix the plugin leads its own process group (see `process_group(0)`
/// at the spawn site in `open`), so the group-wide `SIGKILL` sent here
/// reaches every command the plugin was running on the beam's behalf,
/// not just the plugin binary itself — `Child::start_kill` alone only
/// reaches that one process, which for a real plugin can be little more
/// than a thin wrapper around the actual work. Windows has no
/// process-group equivalent wired up here (see `shell.rs`'s `terminate`
/// doc comment for why this crate does not set one up), so `start_kill`
/// there only ever reaches the immediate plugin process.
async fn kill(child: &mut Child) {
    #[cfg(unix)]
    kill_process_group(child);
    let _ = child.start_kill();
    let _ = child.wait().await;
}
