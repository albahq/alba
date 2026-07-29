//! What a run tells the outside world: the [`RunEvent`] stream a run emits
//! while it happens, and the [`RunSummary`] it ends with.
//!
//! This is the only contract between execution and display: the headless
//! CLI renderer today, and a TUI later, are two consumers of the same
//! stream. Nothing here knows how a
//! beam runs — [`crate::scheduler`] produces these values, and the channel
//! they travel on is unbounded, so a slow consumer never stalls the run.

use std::time::Duration;

use alba_core::BeamId;
use alba_executors::OutputLine;

/// One thing that happened during a run.
///
/// Per beam, the order is `BeamStarted`, then every `BeamOutput`, then
/// `BeamFinished` — a beam that runs several commands still emits exactly
/// one started/finished pair, since the individual commands are an
/// implementation detail of the beam. A cache hit follows the same shape
/// with `BeamCached` in place of `BeamStarted`: the same replayed
/// `BeamOutput` lines, then `BeamFinished` with [`BeamStatus::Cached`]. A
/// beam that never ran (cancelled before it acquired a slot, or skipped
/// because a dependency failed) emits only `BeamFinished` with
/// [`BeamStatus::Cancelled`]. Events from different beams interleave
/// freely; `RunFinished` is last *per run* — in a watch session, it is
/// followed by `WatchWaiting` and, once a change is seen, `WatchTriggered`
/// before the next run's events begin.
#[derive(Debug, Clone)]
pub enum RunEvent {
    BeamStarted {
        id: BeamId,
    },
    /// A cache hit: emitted instead of `BeamStarted` for a beam that is
    /// skipped, followed by its replayed `BeamOutput` lines and a
    /// `BeamFinished` with [`BeamStatus::Cached`] carrying the original
    /// run's duration.
    BeamCached {
        id: BeamId,
    },
    BeamOutput {
        id: BeamId,
        line: OutputLine,
        /// True for a line replayed from the cache rather than produced
        /// by a live command — CI tooling parsing the JSON stream needs
        /// to tell the two apart.
        replayed: bool,
    },
    BeamFinished {
        id: BeamId,
        status: BeamStatus,
        duration: Duration,
    },
    RunFinished {
        summary: RunSummary,
    },
    /// Watch mode only: the session finished a run and is now waiting.
    /// `files` is how many files the watched `inputs` currently resolve
    /// to — the status line's number. The first one a session emits also
    /// announces the session itself.
    WatchWaiting {
        files: usize,
    },
    /// Watch mode only: relevant changes started a new run. `paths` are
    /// project-root-relative display strings; empty means the watcher
    /// overflowed and could not say what changed (rescan). Emitted after
    /// the previous run's `RunFinished` — when that summary is full of
    /// cancelled beams, this event is what marks the run as interrupted
    /// by the watch rather than abandoned by the user.
    WatchTriggered {
        paths: Vec<String>,
    },
}

/// How a beam ended.
///
/// `Failed` and `FailedAllowed` are the same event seen through the beam's
/// `allow_failure` flag: a `FailedAllowed` beam counts as satisfied for its
/// dependents, never triggers fail-fast, and does not affect the run's exit
/// code. `Cancelled` covers both reasons a beam can be skipped: a
/// dependency that failed or was itself cancelled, and a run-wide stop
/// (fail-fast, or the caller's cancellation token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeamStatus {
    Succeeded,
    /// A cache hit: counts as satisfied for this beam's dependents and as
    /// success for the run's exit code, exactly like `Succeeded`.
    Cached,
    Failed {
        exit_code: i32,
    },
    FailedAllowed {
        exit_code: i32,
    },
    Cancelled,
}

/// What a whole run amounts to: every beam of the target's subgraph sorted
/// into the bucket it ended in, plus how long the run took.
///
/// The five vectors are in the project's declaration order, never in
/// completion order — a run is a concurrent thing, and ordering the summary
/// by whichever task happened to finish first would make it differ from run
/// to run for the same project.
#[derive(Debug, Clone, Default)]
pub struct RunSummary {
    pub succeeded: Vec<BeamId>,
    pub cached: Vec<BeamId>,
    pub failed: Vec<BeamId>,
    pub failed_allowed: Vec<BeamId>,
    pub cancelled: Vec<BeamId>,
    pub duration: Duration,
}

impl RunSummary {
    /// The process exit code this run should produce: 0 unless a beam
    /// failed outright. An allowed failure and a cancellation are both
    /// deliberately 0 here; Alba's own errors (a Beamfile that does not
    /// load, an unschedulable run) exit 2 and never reach a summary.
    pub fn exit_code(&self) -> i32 {
        if self.failed.is_empty() { 0 } else { 1 }
    }

    /// Files `id` under `status`. The single place a status becomes a
    /// summary bucket, so the mapping cannot drift between call sites.
    pub(crate) fn record(&mut self, id: BeamId, status: &BeamStatus) {
        match status {
            BeamStatus::Succeeded => self.succeeded.push(id),
            BeamStatus::Cached => self.cached.push(id),
            BeamStatus::Failed { .. } => self.failed.push(id),
            BeamStatus::FailedAllowed { .. } => self.failed_allowed.push(id),
            BeamStatus::Cancelled => self.cancelled.push(id),
        }
    }
}
