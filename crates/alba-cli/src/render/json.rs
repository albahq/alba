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

use std::io;

use alba_engine::{BeamStatus, RunEvent, RunSummary};
use alba_executors::Stream;
use serde::Serialize;

use super::LineSink;

pub struct JsonRenderer {
    out: LineSink<io::Stdout>,
    /// Only ever written to for `ProjectBroken`: the diagnostic still goes
    /// to stderr in JSON mode too, exactly as it did before this event
    /// existed, for whoever is reading the process the old way rather than
    /// parsing the stream.
    err: LineSink<io::Stderr>,
}

impl JsonRenderer {
    pub fn new() -> Self {
        Self {
            out: LineSink::stdout(),
            err: LineSink::stderr(),
        }
    }
}

impl super::Renderer for JsonRenderer {
    fn handle(&mut self, event: &RunEvent) {
        if let RunEvent::ProjectBroken { diagnostic } = event {
            self.err.line(diagnostic.trim_end());
        }
        let wire = WireEvent::from(event);
        // Serializing these types cannot fail: every field is a string, a
        // number, or a sequence of them. The `Result` is still handled
        // rather than unwrapped, since a panic mid-run would be a far
        // worse outcome than a dropped line.
        if let Ok(line) = serde_json::to_string(&wire) {
            self.out.line(&line);
        }
    }
}

/// One line of the JSON stream. The `event` field names the kind, so a
/// consumer can dispatch on it before looking at anything else.
#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum WireEvent<'a> {
    RunStarted {
        target: &'a str,
        beams: Vec<&'a str>,
        edges: Vec<(&'a str, &'a str)>,
    },
    BeamStarted {
        beam: &'a str,
    },
    BeamCached {
        beam: &'a str,
    },
    BeamOutput {
        beam: &'a str,
        stream: WireStream,
        text: &'a str,
        replayed: bool,
    },
    BeamFinished {
        beam: &'a str,
        status: &'static str,
        /// Present only for a beam that actually ran and exited non-zero;
        /// `null` for a success, a cache hit, or a cancellation, which have
        /// no code.
        exit_code: Option<i32>,
        duration_ms: u64,
    },
    RunFinished {
        succeeded: Vec<&'a str>,
        cached: Vec<&'a str>,
        failed: Vec<&'a str>,
        failed_allowed: Vec<&'a str>,
        cancelled: Vec<&'a str>,
        duration_ms: u64,
        /// What the run's beams earned, straight from
        /// `RunSummary::exit_code()`: `0` or `1`. Not always the code the
        /// process ends up returning — a run the user interrupted exits
        /// `130` regardless of how far its beams got.
        exit_code: i32,
    },
    WatchWaiting {
        files: usize,
    },
    WatchTriggered {
        paths: &'a [String],
    },
    ProjectBroken {
        diagnostic: &'a str,
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
            RunEvent::RunStarted {
                target,
                beams,
                edges,
            } => WireEvent::RunStarted {
                target: &target.0,
                beams: beams.iter().map(|id| id.0.as_str()).collect(),
                edges: edges
                    .iter()
                    .map(|(from, to)| (from.0.as_str(), to.0.as_str()))
                    .collect(),
            },
            RunEvent::BeamStarted { id } => WireEvent::BeamStarted { beam: &id.0 },
            RunEvent::BeamCached { id } => WireEvent::BeamCached { beam: &id.0 },
            RunEvent::BeamOutput { id, line, replayed } => WireEvent::BeamOutput {
                beam: &id.0,
                stream: match line.stream {
                    Stream::Stdout => WireStream::Stdout,
                    Stream::Stderr => WireStream::Stderr,
                },
                text: &line.text,
                replayed: *replayed,
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
            RunEvent::WatchWaiting { files } => WireEvent::WatchWaiting { files: *files },
            RunEvent::WatchTriggered { paths } => WireEvent::WatchTriggered { paths },
            RunEvent::ProjectBroken { diagnostic } => WireEvent::ProjectBroken { diagnostic },
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
            cached: ids(&summary.cached),
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
        BeamStatus::Cached => "cached",
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
        BeamStatus::Succeeded | BeamStatus::Cached | BeamStatus::Cancelled => None,
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
            replayed: false,
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
    fn a_cached_beam_emits_its_own_event_and_status() {
        let cached = json(&RunEvent::BeamCached {
            id: BeamId("gen".to_string()),
        });
        assert_eq!(cached["event"], "beam_cached");
        assert_eq!(cached["beam"], "gen");

        let finished = json(&RunEvent::BeamFinished {
            id: BeamId("gen".to_string()),
            status: BeamStatus::Cached,
            duration: std::time::Duration::from_millis(2100),
        });
        assert_eq!(finished["status"], "cached");
        assert!(finished["exit_code"].is_null());
    }

    #[test]
    fn output_lines_carry_the_replayed_marker() {
        let event = |replayed| RunEvent::BeamOutput {
            id: BeamId("gen".to_string()),
            line: OutputLine {
                stream: Stream::Stdout,
                text: "generated".to_string(),
            },
            replayed,
        };

        assert_eq!(json(&event(true))["replayed"], true);
        assert_eq!(json(&event(false))["replayed"], false);
    }

    #[test]
    fn the_final_event_carries_the_cached_bucket() {
        let event = RunEvent::RunFinished {
            summary: RunSummary {
                cached: vec![BeamId("gen".to_string())],
                ..RunSummary::default()
            },
        };
        assert_eq!(json(&event)["cached"][0], "gen");
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

    /// `run_started` is part of the public JSON contract: a consumer needs
    /// the run boundary and the graph snapshot for the same reason the TUI
    /// does.
    #[test]
    fn run_started_is_emitted_on_the_wire() {
        let event = RunEvent::RunStarted {
            target: BeamId("build".to_string()),
            beams: vec![BeamId("codegen".to_string()), BeamId("build".to_string())],
            edges: vec![(BeamId("build".to_string()), BeamId("codegen".to_string()))],
        };

        let line = serde_json::to_string(&WireEvent::from(&event)).unwrap();

        assert_eq!(
            line,
            r#"{"event":"run_started","target":"build","beams":["codegen","build"],"edges":[["build","codegen"]]}"#
        );
    }

    /// The exact wire shape, not just its parsed fields: a consumer's
    /// script matches these strings verbatim, so the field order and the
    /// tag's spelling are the contract, not an incidental detail `json()`'s
    /// round trip through `Value` would hide.
    #[test]
    fn watch_events_serialize_with_their_own_tags() {
        let waiting = RunEvent::WatchWaiting { files: 42 };
        let triggered = RunEvent::WatchTriggered {
            paths: vec!["src/lib.rs".to_string()],
        };

        let waiting = serde_json::to_string(&WireEvent::from(&waiting)).unwrap();
        let triggered = serde_json::to_string(&WireEvent::from(&triggered)).unwrap();

        assert_eq!(waiting, r#"{"event":"watch_waiting","files":42}"#);
        assert_eq!(
            triggered,
            r#"{"event":"watch_triggered","paths":["src/lib.rs"]}"#
        );
    }

    /// `project_broken` is part of the public JSON contract: a consumer
    /// that never reads stderr still needs to learn the session parked and
    /// why.
    #[test]
    fn project_broken_is_emitted_on_the_wire() {
        let event = RunEvent::ProjectBroken {
            diagnostic: "error: oh no\n".to_string(),
        };

        let line = serde_json::to_string(&WireEvent::from(&event)).unwrap();

        assert_eq!(
            line,
            r#"{"event":"project_broken","diagnostic":"error: oh no\n"}"#
        );
    }
}
