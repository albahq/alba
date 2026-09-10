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

/// The user interrupted the run. `128 + SIGINT`, the code a shell reports
/// for an interrupted command — deliberately outside Alba's own 0/1/2
/// vocabulary, which describes how a run *ended*, an answer an abandoned
/// run does not have.
///
/// Both interrupt paths return it, and they must agree: a first Ctrl-C
/// that stops the run in an orderly way (`commands::run::run_exit_code`)
/// and a second one that abandons it outright
/// (`commands::run::watch_interrupts`) are the same event from the user's
/// side, and `alba run deploy && ship` must not ship in either case.
pub const EXIT_INTERRUPTED: i32 = 130;
