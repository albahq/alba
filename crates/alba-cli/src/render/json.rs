//! The JSON renderer: one JSON object per line on stdout, one line per
//! [`RunEvent`]. Selected with `--log-format json`.
//!
//! ## Why the wire types live here and not in `alba-engine`
//!
//! Deriving `Serialize` on the engine's own event types would be less
//! code, but it would put `serde` in `alba-engine` and — worse — make the
//! engine's internal type layout the thing users' scripts parse: renaming
//! a field or adding a `BeamStatus` variant would silently become a
//! breaking change to a published output format. The schema below is the
//! CLI's own public contract, translated from the engine's types at the
//! boundary, so the two can evolve independently and the format is visible
//! in one file rather than spread across `#[serde(...)]` attributes in a
//! crate that has no business knowing about JSON.
//!
//! ## Where the summary goes
//!
//! Unlike the text renderers, this one prints no stderr summary: the
//! `run_finished` line already carries every count, every beam id, the
//! total duration, and the process exit code. Printing a decorated Unicode
//! restatement of it on stderr would be a second, differently-shaped copy
//! of the same facts for a consumer that by construction is parsing the
//! first one.

use alba_engine::{BeamStatus, RunEvent, RunSummary};
use alba_executors::Stream;
use serde::Serialize;

#[derive(Default)]
pub struct JsonRenderer;

impl JsonRenderer {
    pub fn new() -> Self {
        Self
    }
}

impl super::Renderer for JsonRenderer {
    fn handle(&mut self, event: &RunEvent) {
        let wire = WireEvent::from(event);
        // Serializing these types cannot fail: every field is a string, a
        // number, or a sequence of them. The `Result` is still handled
        // rather than unwrapped, since a panic mid-run would be a far
        // worse outcome than a dropped line.
        if let Ok(line) = serde_json::to_string(&wire) {
            println!("{line}");
        }
    }
}

/// One line of the JSON stream. The `event` field names the kind, so a
/// consumer can dispatch on it before looking at anything else.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WireEvent<'a> {
    BeamStarted {
        beam: &'a str,
    },
    BeamOutput {
        beam: &'a str,
        stream: WireStream,
        text: &'a str,
    },
    BeamFinished {
        beam: &'a str,
        status: &'static str,
        /// Present only for a beam that actually ran and exited non-zero;
        /// `null` for a success or a cancellation, which have no code.
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    RunFinished {
        succeeded: Vec<&'a str>,
        failed: Vec<&'a str>,
        failed_allowed: Vec<&'a str>,
        cancelled: Vec<&'a str>,
        duration_ms: u64,
        /// The code the `alba` process is about to exit with.
        exit_code: i32,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum WireStream {
    Stdout,
    Stderr,
}

impl<'a> From<&'a RunEvent> for WireEvent<'a> {
    fn from(event: &'a RunEvent) -> Self {
        match event {
            RunEvent::BeamStarted { id } => WireEvent::BeamStarted { beam: &id.0 },
            RunEvent::BeamOutput { id, line } => WireEvent::BeamOutput {
                beam: &id.0,
                stream: match line.stream {
                    Stream::Stdout => WireStream::Stdout,
                    Stream::Stderr => WireStream::Stderr,
                },
                text: &line.text,
            },
            RunEvent::BeamFinished {
                id,
                status,
                duration,
            } => WireEvent::BeamFinished {
                beam: &id.0,
                status: status_name(status),
                exit_code: exit_code(status),
                duration_ms: millis(duration),
            },
            RunEvent::RunFinished { summary } => WireEvent::from_summary(summary),
        }
    }
}

impl<'a> WireEvent<'a> {
    fn from_summary(summary: &'a RunSummary) -> Self {
        let ids = |bucket: &'a [alba_core::BeamId]| -> Vec<&'a str> {
            bucket.iter().map(|id| id.0.as_str()).collect()
        };
        WireEvent::RunFinished {
            succeeded: ids(&summary.succeeded),
            failed: ids(&summary.failed),
            failed_allowed: ids(&summary.failed_allowed),
            cancelled: ids(&summary.cancelled),
            duration_ms: millis(&summary.duration),
            exit_code: summary.exit_code(),
        }
    }
}

fn status_name(status: &BeamStatus) -> &'static str {
    match status {
        BeamStatus::Succeeded => "succeeded",
        BeamStatus::Failed { .. } => "failed",
        BeamStatus::FailedAllowed { .. } => "failed_allowed",
        BeamStatus::Cancelled => "cancelled",
    }
}

fn exit_code(status: &BeamStatus) -> Option<i32> {
    match status {
        BeamStatus::Failed { exit_code } | BeamStatus::FailedAllowed { exit_code } => {
            Some(*exit_code)
        }
        BeamStatus::Succeeded | BeamStatus::Cancelled => None,
    }
}

/// A duration in whole milliseconds. `as_millis` is a `u128`, which no
/// realistic run needs: a `u64` covers 584 million years and keeps the
/// JSON a plain integer every parser handles.
fn millis(duration: &std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_executors::OutputLine;

    fn json(event: &RunEvent) -> serde_json::Value {
        serde_json::from_str(&serde_json::to_string(&WireEvent::from(event)).unwrap()).unwrap()
    }

    #[test]
    fn output_carries_the_stream_it_came_from() {
        let event = RunEvent::BeamOutput {
            id: BeamId("build".to_string()),
            line: OutputLine {
                stream: Stream::Stderr,
                text: "warning".to_string(),
            },
        };

        let value = json(&event);

        assert_eq!(value["event"], "beam_output");
        assert_eq!(value["beam"], "build");
        assert_eq!(value["stream"], "stderr");
        assert_eq!(value["text"], "warning");
    }

    #[test]
    fn a_failure_reports_its_exit_code_and_a_success_reports_none() {
        let finished = |status| RunEvent::BeamFinished {
            id: BeamId("build".to_string()),
            status,
            duration: std::time::Duration::from_millis(1500),
        };

        let failed = json(&finished(BeamStatus::Failed { exit_code: 7 }));
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], 7);
        assert_eq!(failed["duration_ms"], 1500);

        let succeeded = json(&finished(BeamStatus::Succeeded));
        assert_eq!(succeeded["status"], "succeeded");
        assert!(succeeded["exit_code"].is_null());
    }

    #[test]
    fn the_final_event_carries_every_bucket_and_the_exit_code() {
        let event = RunEvent::RunFinished {
            summary: RunSummary {
                succeeded: vec![BeamId("a".to_string())],
                failed: vec![BeamId("b".to_string())],
                ..RunSummary::default()
            },
        };

        let value = json(&event);

        assert_eq!(value["event"], "run_finished");
        assert_eq!(value["succeeded"][0], "a");
        assert_eq!(value["failed"][0], "b");
        assert_eq!(value["exit_code"], 1);
    }
}
