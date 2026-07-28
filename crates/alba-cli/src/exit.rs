//! Process exit code constants for the `alba` binary.
//!
//! Success is always plain `0` (no named constant: every command's happy
//! path already returns `0` directly, and a third one-variant name would
//! not pull its weight). The two failure codes are named so a call site
//! reads as "a beam failed" or "Alba itself failed" rather than a bare
//! integer.

/// A beam's command exited non-zero (and `allow_failure` did not cover
/// it), or a run was cancelled because another beam failed. Reserved for
/// `alba run`, added in a later task; nothing in this task returns it yet,
/// hence the explicit `allow` rather than leaving it to trip
/// `-D warnings` in the meantime.
#[allow(dead_code)]
pub const EXIT_BEAM_FAILURE: i32 = 1;

/// Alba itself failed before or instead of running a beam's commands: a
/// missing Beamfile, a parse or validation error, a bad invocation.
pub const EXIT_ALBA_ERROR: i32 = 2;
