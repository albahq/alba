//! Renderers: the three ways a run's [`RunEvent`] stream is presented.
//!
//! A renderer is a plain [`Renderer::handle`] per event, called from a
//! single task that owns the receiving end of the engine's channel — so an
//! implementation never needs to synchronize anything, and events reach it
//! in exactly the order the engine emitted them.
//!
//! ## Which stream output goes to
//!
//! The two text renderers write *everything* they produce — line prefixes,
//! group headers, and beam output from both `stdout` and `stderr` — to the
//! process's stdout. Alba's own commentary (the run summary, the
//! `cancelling...` announcement, error diagnostics) goes to stderr. The
//! split is "the run's output" versus "Alba talking about the run", not
//! "the beam's stdout" versus "the beam's stderr": a prefix or a group
//! header has to stay contiguous with the lines it frames, which two
//! independently buffered streams cannot guarantee. The stream a line came
//! from is preserved losslessly by [`json::JsonRenderer`], which is the
//! mode built for a consumer that cares.

use std::time::Duration;

use alba_engine::{BeamStatus, RunEvent, RunSummary};

mod grouped;
mod interleaved;
mod json;

pub use grouped::GroupedRenderer;
pub use interleaved::InterleavedRenderer;
pub use json::JsonRenderer;

/// A consumer of a run's event stream.
///
/// `Send` because the only caller drives it from a spawned task running
/// concurrently with the engine, so output is streamed while beams are
/// still running rather than buffered until the run ends.
pub trait Renderer: Send {
    fn handle(&mut self, event: &RunEvent);
}

/// How long something took, as one decimal of a second (`4.1s`).
///
/// Deliberately the same shape at every magnitude — no minute/hour
/// breakdown — because the only thing that would buy is prettiness at
/// durations Alba's own tests never produce, and a second format is a
/// second thing to keep stable across platforms. Nothing in the test suite
/// asserts a duration's value, only that one is present.
pub fn format_duration(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

/// How a beam ended, as it appears in a text renderer's status line.
///
/// `failed (allowed)` deliberately omits the exit code that `failed`
/// reports: an allowed failure does not affect the run, so the number is
/// noise, and the phrase is what the user greps for.
pub fn status_label(status: &BeamStatus) -> String {
    match status {
        BeamStatus::Succeeded => "ok".to_string(),
        BeamStatus::Failed { exit_code } => format!("failed (exit {exit_code})"),
        BeamStatus::FailedAllowed { .. } => "failed (allowed)".to_string(),
        BeamStatus::Cancelled => "cancelled".to_string(),
    }
}

/// Prints the one-line run summary on stderr, e.g.
/// `✓ 3 succeeded · ✗ 1 failed · ⊘ 2 cancelled · 4.1s`.
///
/// Empty buckets are omitted rather than reported as zero: a run where
/// nothing failed should not spend half its summary saying so. The total
/// duration is always last, so the line is never empty even for a run
/// whose every bucket happens to be empty.
pub fn print_summary(summary: &RunSummary) {
    let counts = [
        ("\u{2713}", summary.succeeded.len(), "succeeded"),
        ("\u{2717}", summary.failed.len(), "failed"),
        ("\u{26a0}", summary.failed_allowed.len(), "failed (allowed)"),
        ("\u{2298}", summary.cancelled.len(), "cancelled"),
    ];

    let mut parts: Vec<String> = counts
        .iter()
        .filter(|(_, count, _)| *count > 0)
        .map(|(mark, count, label)| format!("{mark} {count} {label}"))
        .collect();
    parts.push(format_duration(summary.duration));

    eprintln!("{}", parts.join(" \u{b7} "));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_with_one_decimal() {
        assert_eq!(format_duration(Duration::from_millis(1234)), "1.2s");
        assert_eq!(format_duration(Duration::ZERO), "0.0s");
    }

    #[test]
    fn labels_a_failure_with_its_exit_code_but_an_allowed_one_without() {
        assert_eq!(
            status_label(&BeamStatus::Failed { exit_code: 7 }),
            "failed (exit 7)"
        );
        assert_eq!(
            status_label(&BeamStatus::FailedAllowed { exit_code: 1 }),
            "failed (allowed)"
        );
    }
}
