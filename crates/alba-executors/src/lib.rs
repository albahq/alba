//! Executors that run beam actions on a target platform.
//!
//! This crate is deliberately at the bottom of Alba's dependency graph: it
//! does not depend on `alba-core` or `alba-syntax`. A [`CommandSpec`]
//! carries an already-rendered command line — template rendering happens
//! upstream, in the engine, before a command ever reaches an [`ExecSession`].
//!
//! The [`ExecSession`] a beam runs in is the per-beam unit: [`Executor::open`]
//! and [`ExecSession::close`] bracket the whole set of commands one beam
//! declares, so a session can hold state — a running container, a plugin
//! process — that outlives any single command and is shared across the
//! beam's commands. The engine owns the bracket: it opens exactly one
//! session per beam before its first command, runs every command through
//! it, and closes it once, in every case (success, failure, or
//! cancellation) — cleanup is the engine's responsibility, never a
//! session's own.
//!
//! [`EmbeddedShellExecutor`] runs commands through `alba-shell`, Alba's own
//! POSIX-like interpreter, in-process — the default. [`SystemShellExecutor`]
//! runs them through the host shell (`sh -c` on unix, `powershell
//! -NoProfile -Command` on windows), the per-beam opt-out via `executor
//! system_shell`. [`FakeExecutor`], behind the `test-util` feature, is a
//! scriptable test double the scheduler's own tests drive to assert
//! dependency order, parallelism, and cancellation without spawning real
//! processes.

use std::path::PathBuf;

mod docker;
mod embedded;
mod plugin;
mod shell;

#[cfg(feature = "test-util")]
mod fake;

pub mod protocol;

pub use docker::DockerExecutor;
pub use embedded::EmbeddedShellExecutor;
pub use plugin::PluginExecutor;
pub use shell::SystemShellExecutor;

#[cfg(feature = "test-util")]
pub use fake::{FakeBehavior, FakeEvent, FakeExecutor};

/// Opens the session a beam's commands run in.
///
/// Implementations are used behind `Arc<dyn Executor>` and must be
/// `Send + Sync`; `open` takes `&self` so a single executor instance can
/// open many sessions concurrently, one per beam. See [`ExecContext::cancel`]
/// for the cancellation contract, including its per-platform limits.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError>;
}

/// The per-beam unit a session's commands run in: whatever state a beam
/// needs across its whole `run` list — a container, a plugin process —
/// lives here, opened once before the first command and closed once after
/// the last (or after whichever command failed or was cancelled).
#[async_trait::async_trait]
pub trait ExecSession: Send {
    async fn execute(
        &mut self,
        cmd: CommandSpec,
        ctx: ExecContext,
    ) -> Result<ExecResult, ExecError>;
    async fn close(self: Box<Self>) -> Result<(), ExecError>;
}

/// Everything a session needs to exist before its first command: the
/// beam's label (names containers and plugin processes), its directory,
/// the executor options as JSON (the engine serializes `ExecutorKind` into
/// this; `Null` for the shell executors), an output channel for setup-time
/// lines (an image pull), and the run's cancellation token.
pub struct BeamContext {
    pub beam: String,
    pub dir: PathBuf,
    pub options: serde_json::Value,
    pub output: tokio::sync::mpsc::UnboundedSender<OutputLine>,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// One already-rendered command line, ready to hand to a shell.
///
/// `env` is applied on top of the executing process's own environment: it
/// extends and overrides it (matching variables win, everything else the
/// parent process sees stays available) rather than replacing it wholesale.
/// Consumers (the engine, the CLI) can rely on `PATH` and other ambient
/// variables being present even when `env` is empty.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub command: String,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
}

/// Per-invocation channels: where output lines go, and how the caller asks
/// the running command to stop.
pub struct ExecContext {
    pub output: tokio::sync::mpsc::UnboundedSender<OutputLine>,
    /// Cancelling this token asks the running command to stop: on unix,
    /// [`SystemShellExecutor`] sends `SIGTERM` to the whole process group
    /// the command runs in (so a compound or backgrounding command dies
    /// together, not just its immediate shell process), waits a grace
    /// period, then escalates to a group-wide `SIGKILL`. On windows there
    /// is no process-group equivalent wired up (that needs a Job object,
    /// which this crate does not create), so cancellation there is an
    /// immediate, unconditional kill of the immediate child process only —
    /// a windows beam command that backgrounds a descendant can outlive
    /// cancellation.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// One line of output, already split on newlines.
///
/// A trailing `\r` (as produced by `powershell` on windows) is stripped
/// along with the `\n` it precedes; a final line with no trailing newline
/// is still emitted. Stdout and stderr are read by two independent tasks,
/// so lines from the two streams can arrive interleaved in either relative
/// order — callers must not depend on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: Stream,
    pub text: String,
}

/// Which stream an [`OutputLine`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// The outcome of a command that ran to completion (including a command
/// that was cancelled and terminated — its exit code is still reported
/// here, not as an [`ExecError`]).
///
/// `exit_code` is `-1` when the underlying platform reports no discrete
/// exit code at all (for example a process killed by a signal on unix).
/// This is a fallback value, not a reserved sentinel: `-1` is also a
/// legitimate exit code a process can return on its own on windows, so
/// callers cannot use `exit_code == -1` alone to distinguish "the process
/// was killed" from "the process chose to exit with -1".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: i32,
}

/// An executor-level failure: the command never produced an exit code at
/// all (for example, the shell binary could not be spawned).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ExecError {
    pub message: String,
}
