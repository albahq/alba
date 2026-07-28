//! Process exit code constants for the `alba` binary.
//!
//! Only the codes this binary *decides for itself* live here. Success
//! (`0`) and beam failure (`1`) are not among them: both now come straight
//! out of `alba_engine::RunSummary::exit_code()`, which owns the mapping
//! from a run's outcome to its code. The `EXIT_BEAM_FAILURE` placeholder
//! that stood here in anticipation of `alba run` is gone for that reason —
//! it was never read, and a second name for a number the engine decides
//! is a second place for that decision to drift.

/// Alba itself failed before or instead of running a beam's commands: a
/// missing Beamfile, a parse or validation error, a bad invocation, an
/// unschedulable run.
pub const EXIT_ALBA_ERROR: i32 = 2;

/// A run was abandoned mid-flight on a second Ctrl-C, rather than allowed
/// to stop in an orderly way. `128 + SIGINT`, the code a shell reports for
/// an interrupted command — deliberately outside Alba's own 0/1/2
/// vocabulary, which describes how a run *ended*, an answer an abandoned
/// run does not have. See `commands::run::watch_interrupts`.
pub const EXIT_INTERRUPTED: i32 = 130;
