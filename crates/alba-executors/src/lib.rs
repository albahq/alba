//! Executors that run beam actions on a target platform.
//!
//! This crate is deliberately at the bottom of Alba's dependency graph: it
//! does not depend on `alba-core` or `alba-syntax`. A [`CommandSpec`]
//! carries an already-rendered command line — template rendering happens
//! upstream, in the engine, before a command ever reaches an [`Executor`].
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

mod embedded;
mod shell;

#[cfg(feature = "test-util")]
mod fake;

pub use embedded::EmbeddedShellExecutor;
pub use shell::SystemShellExecutor;

#[cfg(feature = "test-util")]
pub use fake::{FakeBehavior, FakeExecutor};

/// Runs a single already-rendered command to completion.
///
/// Implementations are used behind `Arc<dyn Executor>` and must be
/// `Send + Sync`; `execute` takes `&self` so a single executor instance can
/// run many commands concurrently. See [`ExecContext::cancel`] for the
/// cancellation contract, including its per-platform limits.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    async fn execute(&self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError>;
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
