//! Orchestration engine that schedules and runs beams.
//!
//! `alba-engine` sits between the model and the machine: it takes the
//! validated [`alba_core::Project`] the loader produced, extracts the
//! target beam's execution subgraph, and runs it with bounded parallelism
//! through an [`alba_executors::Executor`] — which it only ever sees as
//! that trait, so a future docker or plugin executor drops in without this
//! crate changing. See the `scheduler` module for the scheduling rules
//! themselves.
//!
//! Everything a caller needs is [`run`]: give it a project, a target,
//! [`RunOptions`], an executor, a channel to receive [`RunEvent`]s on, and
//! a cancellation token, and it reports back a [`RunSummary`].

mod event;
mod scheduler;

pub use event::{BeamStatus, RunEvent, RunSummary};
pub use scheduler::{RunOptions, run};

use alba_core::{BeamId, CoreError};
use alba_executors::ExecError;

/// Something that stopped Alba from running the beams as asked.
///
/// Deliberately *not* how a failing beam is reported: a command that exits
/// non-zero is an expected outcome of a run and lands in
/// [`RunSummary::failed`]. An `EngineError` means the run could not be
/// carried out at all, and is what the CLI turns into its exit code 2.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The project could not answer a question the engine asked of it: an
    /// unknown target beam, or a `run`/`env` template that failed to
    /// render at schedule time. Carries the span and source file the CLI
    /// needs to point at the offending Beamfile.
    #[error(transparent)]
    Core(#[from] CoreError),
    /// The run cannot be scheduled as requested: a beam using an executor
    /// Alba does not implement yet, or parameters that do not match what
    /// the target beam declares. Detected before any beam starts.
    #[error("{0}")]
    Unschedulable(String),
    /// An executor could not run a command at all — no exit code was ever
    /// produced (a shell that could not be spawned, say), which is an
    /// Alba-level failure rather than a beam that failed.
    #[error("beam `{}`: {source}", beam.0)]
    Executor {
        beam: BeamId,
        #[source]
        source: ExecError,
    },
    /// A beam's task panicked, which can only be a bug in the engine or in
    /// an `Executor` implementation. Reported rather than unwrapped, so
    /// one broken beam cannot take the whole run down silently.
    #[error("beam `{}` panicked while running", beam.0)]
    Panicked { beam: BeamId },
}
