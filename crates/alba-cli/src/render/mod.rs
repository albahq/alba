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
//!
//! ## Every line goes through a [`LineSink`]
//!
//! Nothing here uses `println!`/`eprintln!`. See [`LineSink`] for why: a
//! run whose reader walked away (`alba run x | head -1`) must still finish,
//! still report, and still exit with the code its beams earned.

use std::io::{self, Write};
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

/// A line-oriented writer that gives up quietly once its reader is gone.
///
/// `println!` and `eprintln!` *panic* when the write fails, and the write
/// fails as soon as the other end of a pipe closes. `alba run x | head -1`
/// is a pipeline users write without thinking about it, and under the
/// macros it panicked the renderer task mid-run: the summary was never
/// printed, and the process ended up reporting success regardless of what
/// the beams actually did.
///
/// So every line a renderer emits goes through here instead. The first
/// failed write latches the sink closed and every later write is a cheap
/// no-op — the run keeps going to completion with nobody left to read it,
/// and [`crate::commands::run`] still returns the code the run earned.
/// Any write error latches, not only `BrokenPipe`: a stdout that cannot be
/// written to is not a condition Alba can recover from or usefully report
/// (the report would go to the same broken stream), and retrying it once
/// per line for the rest of the run would be worse than silence.
pub struct LineSink<W: Write> {
    writer: W,
    open: bool,
}

impl<W: Write> LineSink<W> {
    pub fn new(writer: W) -> Self {
        Self { writer, open: true }
    }

    /// Writes `text` followed by a newline, or does nothing if this sink
    /// has already been closed by a failed write.
    pub fn line(&mut self, text: &str) {
        if !self.open {
            return;
        }
        if writeln!(self.writer, "{text}").is_err() {
            self.open = false;
        }
    }
}

impl LineSink<io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(io::stdout())
    }
}

impl LineSink<io::Stderr> {
    pub fn stderr() -> Self {
        Self::new(io::stderr())
    }
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
        BeamStatus::Cached => "cached".to_string(),
        BeamStatus::Failed { exit_code } => format!("failed (exit {exit_code})"),
        BeamStatus::FailedAllowed { .. } => "failed (allowed)".to_string(),
        BeamStatus::Cancelled => "cancelled".to_string(),
    }
}

/// Writes the one-line run summary to `sink` (stderr), e.g.
/// `✓ 3 succeeded · ✗ 1 failed · ⊘ 2 cancelled · 4.1s`.
///
/// Empty buckets are omitted rather than reported as zero: a run where
/// nothing failed should not spend half its summary saying so. The total
/// duration is always last, so the line is never empty even for a run
/// whose every bucket happens to be empty.
pub fn print_summary(sink: &mut LineSink<io::Stderr>, summary: &RunSummary) {
    sink.line(&summary_line(summary));
}

/// The summary's text, split out from the writing so it can be asserted
/// directly rather than through a captured stream.
fn summary_line(summary: &RunSummary) -> String {
    let counts = [
        ("\u{2713}", summary.succeeded.len(), "succeeded"),
        ("\u{21ba}", summary.cached.len(), "cached"),
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

    parts.join(" \u{b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;

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

    #[test]
    fn a_cached_beam_is_labelled_cached() {
        assert_eq!(status_label(&BeamStatus::Cached), "cached");
    }

    #[test]
    fn the_summary_counts_cached_beams() {
        let summary = RunSummary {
            succeeded: vec![BeamId("a".to_string())],
            cached: vec![BeamId("b".to_string()), BeamId("c".to_string())],
            duration: Duration::from_millis(4100),
            ..RunSummary::default()
        };

        assert_eq!(
            summary_line(&summary),
            "\u{2713} 1 succeeded \u{b7} \u{21ba} 2 cached \u{b7} 4.1s"
        );
    }

    #[test]
    fn the_summary_omits_empty_buckets_and_always_ends_with_the_duration() {
        let summary = RunSummary {
            succeeded: vec![BeamId("a".to_string())],
            cancelled: vec![BeamId("b".to_string()), BeamId("c".to_string())],
            duration: Duration::from_millis(4100),
            ..RunSummary::default()
        };

        assert_eq!(
            summary_line(&summary),
            "\u{2713} 1 succeeded \u{b7} \u{2298} 2 cancelled \u{b7} 4.1s"
        );
    }

    /// A writer whose every write fails, standing in for the closed end of
    /// a pipe: the sink must swallow it rather than panic, and must not
    /// keep trying afterwards.
    struct BrokenWriter {
        attempts: usize,
    }

    impl Write for BrokenWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "broken pipe"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_broken_sink_swallows_the_failure_and_stops_writing() {
        let mut sink = LineSink::new(BrokenWriter { attempts: 0 });

        sink.line("first");
        sink.line("second");
        sink.line("third");

        assert_eq!(
            sink.writer.attempts, 1,
            "the sink must latch closed after the first failure"
        );
    }

    #[test]
    fn an_open_sink_writes_every_line() {
        let mut sink = LineSink::new(Vec::new());

        sink.line("first");
        sink.line("second");

        assert_eq!(String::from_utf8(sink.writer).unwrap(), "first\nsecond\n");
    }
}
