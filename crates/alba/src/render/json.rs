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

use std::io::{self, Write};

use alba_core::{ChangeMode, FullReason};
use alba_engine::{BeamStatus, RunEvent, RunSummary};
use alba_executors::Stream;
use serde::Serialize;

use super::LineSink;

pub struct JsonRenderer<O: Write = io::Stdout, E: Write = io::Stderr> {
    out: LineSink<O>,
    /// Only ever written to for `ProjectBroken`: the diagnostic still goes
    /// to stderr in JSON mode too, exactly as it did before this event
    /// existed, for whoever is reading the process the old way rather than
    /// parsing the stream.
    err: LineSink<E>,
}

impl JsonRenderer {
    pub fn new() -> Self {
        Self::with_writers(io::stdout(), io::stderr())
    }
}

impl<O: Write, E: Write> JsonRenderer<O, E> {
    pub fn with_writers(out: O, err: E) -> Self {
        Self {
            out: LineSink::new(out),
            err: LineSink::new(err),
        }
    }
}

impl<O: Write + Send, E: Write + Send> super::Renderer for JsonRenderer<O, E> {
    fn handle(&mut self, event: &RunEvent) {
        if let RunEvent::ProjectBroken { diagnostic, .. } = event {
            self.err.line(diagnostic.trim_end());
        }
        let Some(wire) = WireEvent::of(event) else {
            return;
        };
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
        targets: Vec<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        affected_by: Option<&'a str>,
        /// The reference `git.changes()` diffs from and its merge-base
        /// commit; absent when the run has none.
        #[serde(skip_serializing_if = "Option::is_none")]
        base: Option<WireBase<'a>>,
        beams: Vec<&'a str>,
        edges: Vec<(&'a str, &'a str)>,
    },
    BeamStarted {
        beam: &'a str,
        /// `incremental` or `full`, only for a beam calling
        /// `git.changes()`.
        #[serde(skip_serializing_if = "Option::is_none")]
        mode: Option<&'static str>,
        /// Why the mode is `full`; absent otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        full_reason: Option<&'static str>,
        /// What `full_reason` names, when it names something: the changed
        /// path (relative to the project root) for `input_outside_changes`,
        /// the dependency for `dependency_affected`; absent otherwise.
        #[serde(skip_serializing_if = "Option::is_none")]
        full_reason_detail: Option<&'a str>,
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
        /// Beams with no changed file to process; absent when there are
        /// none, so a run without `git.changes()` keeps its shape.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        unchanged: Vec<&'a str>,
        failed: Vec<&'a str>,
        failed_allowed: Vec<&'a str>,
        cancelled: Vec<&'a str>,
        duration_ms: u64,
        /// What the run's beams earned, straight from
        /// `RunSummary::exit_code()`: `0` or `1`. Not always the code the
        /// process ends up returning — a run the user interrupted exits
        /// `130` regardless of how far its beams got.
        exit_code: i32,
        /// Errors met while releasing what executors held for the run;
        /// always present, empty when nothing failed.
        cleanup_errors: &'a [String],
    },
    WatchWaiting {
        files: usize,
    },
    WatchTriggered {
        paths: &'a [String],
    },
    /// Something the run warns about without stopping, once per run.
    Warning {
        message: &'a str,
    },
    ProjectBroken {
        diagnostic: &'a str,
    },
}

#[derive(Serialize)]
struct WireBase<'a> {
    reference: &'a str,
    merge_base: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum WireStream {
    Stdout,
    Stderr,
}

impl<'a> WireEvent<'a> {
    /// The line an event becomes, or `None` for the events this stream
    /// does not carry.
    fn of(event: &'a RunEvent) -> Option<Self> {
        Some(match event {
            RunEvent::RunStarted {
                targets,
                affected_by,
                base,
                beams,
                edges,
                details: _,
            } => WireEvent::RunStarted {
                targets: targets.iter().map(|id| id.0.as_str()).collect(),
                affected_by: affected_by.as_deref(),
                base: base.as_ref().map(|(reference, merge_base)| WireBase {
                    reference,
                    merge_base,
                }),
                beams: beams.iter().map(|id| id.0.as_str()).collect(),
                edges: edges
                    .iter()
                    .map(|(from, to)| (from.0.as_str(), to.0.as_str()))
                    .collect(),
            },
            RunEvent::BeamStarted { id, mode } => WireEvent::BeamStarted {
                beam: &id.0,
                mode: mode.as_ref().map(mode_name),
                full_reason: mode.as_ref().and_then(full_reason_name),
                full_reason_detail: mode.as_ref().and_then(full_reason_detail),
            },
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
            // A notice keeps the wire shape it always had, a stderr line.
            RunEvent::BeamNotice { id, text, .. } => WireEvent::BeamOutput {
                beam: &id.0,
                stream: WireStream::Stderr,
                text,
                replayed: false,
            },
            // The stream keeps its event kinds: nothing is written for it.
            RunEvent::CommandStarted { .. } => return None,
            RunEvent::WatchWaiting { files } => WireEvent::WatchWaiting { files: *files },
            RunEvent::WatchTriggered { paths } => WireEvent::WatchTriggered { paths },
            RunEvent::ProjectBroken { diagnostic, .. } => WireEvent::ProjectBroken { diagnostic },
            RunEvent::Warning { message } => WireEvent::Warning { message },
        })
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
            unchanged: ids(&summary.unchanged),
            failed: ids(&summary.failed),
            failed_allowed: ids(&summary.failed_allowed),
            cancelled: ids(&summary.cancelled),
            duration_ms: millis(&summary.duration),
            exit_code: summary.exit_code(),
            cleanup_errors: &summary.cleanup_errors,
        }
    }
}

fn status_name(status: &BeamStatus) -> &'static str {
    match status {
        BeamStatus::Succeeded => "succeeded",
        BeamStatus::Cached => "cached",
        BeamStatus::Unchanged => "unchanged",
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
        BeamStatus::Succeeded
        | BeamStatus::Cached
        | BeamStatus::Unchanged
        | BeamStatus::Cancelled => None,
    }
}

fn mode_name(mode: &ChangeMode) -> &'static str {
    match mode {
        ChangeMode::Incremental => "incremental",
        ChangeMode::Full(_) => "full",
    }
}

fn full_reason_name(mode: &ChangeMode) -> Option<&'static str> {
    let ChangeMode::Full(reason) = mode else {
        return None;
    };
    Some(match reason {
        FullReason::NoReference => "no_reference",
        FullReason::ReferenceUnresolved => "reference_unresolved",
        FullReason::OnTargetBranch => "on_target_branch",
        FullReason::BeamfileChanged => "beamfile_changed",
        FullReason::DependencyAffected(_) => "dependency_affected",
        FullReason::InputOutsideChanges(_) => "input_outside_changes",
    })
}

/// What a full mode's reason names, for the reasons that name something.
fn full_reason_detail(mode: &ChangeMode) -> Option<&str> {
    match mode {
        ChangeMode::Full(FullReason::InputOutsideChanges(path)) => Some(path),
        ChangeMode::Full(FullReason::DependencyAffected(id)) => Some(&id.0),
        _ => None,
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
        serde_json::from_str(&serde_json::to_string(&WireEvent::of(event).unwrap()).unwrap())
            .unwrap()
    }

    #[test]
    fn command_started_writes_nothing() {
        let event = RunEvent::CommandStarted {
            id: BeamId("build".to_string()),
            command: "cargo build".to_string(),
        };

        let mut renderer = JsonRenderer::with_writers(Vec::new(), Vec::new());

        super::super::Renderer::handle(&mut renderer, &event);
        assert!(renderer.out.writer.is_empty(), "nothing is written");

        // The sink itself works: another event does write a line.
        super::super::Renderer::handle(
            &mut renderer,
            &RunEvent::BeamStarted {
                id: BeamId("build".to_string()),
                mode: None,
            },
        );
        assert!(!renderer.out.writer.is_empty());
    }

    #[test]
    fn a_notice_is_written_as_a_stderr_output_event() {
        let event = RunEvent::BeamNotice {
            id: BeamId("build".to_string()),
            level: alba_engine::NoticeLevel::Error,
            text: "failed to spawn".to_string(),
        };

        let mut renderer = JsonRenderer::with_writers(Vec::new(), Vec::new());
        super::super::Renderer::handle(&mut renderer, &event);

        let written = String::from_utf8(renderer.out.writer.clone()).unwrap();
        assert_eq!(
            written.trim_end(),
            r#"{"event":"beam_output","beam":"build","stream":"stderr","text":"failed to spawn","replayed":false}"#
        );
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

    #[test]
    fn the_final_event_carries_the_cleanup_errors() {
        let event = RunEvent::RunFinished {
            summary: RunSummary {
                cleanup_errors: vec!["db would not stop".to_string()],
                ..RunSummary::default()
            },
        };

        let value = json(&event);

        assert_eq!(
            value["cleanup_errors"],
            serde_json::json!(["db would not stop"])
        );
        assert_eq!(value["exit_code"], 1);
    }

    #[test]
    fn the_final_event_carries_an_empty_cleanup_errors_field_when_nothing_failed() {
        let event = RunEvent::RunFinished {
            summary: RunSummary::default(),
        };

        assert_eq!(json(&event)["cleanup_errors"], serde_json::json!([]));
    }

    #[test]
    fn an_unchanged_beam_is_reported_with_its_status_and_bucket() {
        let finished = json(&RunEvent::BeamFinished {
            id: BeamId("phpcs".to_string()),
            status: BeamStatus::Unchanged,
            duration: std::time::Duration::ZERO,
        });
        assert_eq!(finished["status"], "unchanged");
        assert!(finished["exit_code"].is_null());

        let run_finished = json(&RunEvent::RunFinished {
            summary: RunSummary {
                unchanged: vec![BeamId("phpcs".to_string())],
                ..RunSummary::default()
            },
        });
        assert_eq!(run_finished["unchanged"], serde_json::json!(["phpcs"]));
        assert_eq!(run_finished["exit_code"], 0);
    }

    /// A run with nothing unchanged keeps the shape it always had.
    #[test]
    fn the_unchanged_bucket_is_omitted_when_empty() {
        let value = json(&RunEvent::RunFinished {
            summary: RunSummary::default(),
        });
        assert!(value.get("unchanged").is_none(), "{value}");
    }

    #[test]
    fn beam_started_carries_the_mode_and_why_it_is_full() {
        let started = |mode| {
            json(&RunEvent::BeamStarted {
                id: BeamId("phpcs".to_string()),
                mode,
            })
        };

        let incremental = started(Some(ChangeMode::Incremental));
        assert_eq!(incremental["mode"], "incremental");
        assert!(incremental.get("full_reason").is_none());

        let without = started(None);
        assert!(without.get("mode").is_none());
        assert!(without.get("full_reason").is_none());

        for (reason, name) in [
            (FullReason::NoReference, "no_reference"),
            (FullReason::ReferenceUnresolved, "reference_unresolved"),
            (FullReason::OnTargetBranch, "on_target_branch"),
            (FullReason::BeamfileChanged, "beamfile_changed"),
            (
                FullReason::DependencyAffected(BeamId("install".to_string())),
                "dependency_affected",
            ),
            (
                FullReason::InputOutsideChanges("phpcs.xml".to_string()),
                "input_outside_changes",
            ),
        ] {
            let full = started(Some(ChangeMode::Full(reason)));
            assert_eq!(full["mode"], "full");
            assert_eq!(full["full_reason"], name);
        }
    }

    #[test]
    fn run_started_carries_the_base_when_there_is_one() {
        let event = RunEvent::RunStarted {
            targets: vec![BeamId("phpcs".to_string())],
            affected_by: None,
            base: Some(("origin/develop".to_string(), "abc123".to_string())),
            beams: vec![BeamId("phpcs".to_string())],
            edges: Vec::new(),
            details: Vec::new(),
        };

        assert_eq!(
            json(&event)["base"],
            serde_json::json!({"reference": "origin/develop", "merge_base": "abc123"})
        );
    }

    /// `run_started` is part of the public JSON contract: a consumer needs
    /// the run boundary and the graph snapshot for the same reason the TUI
    /// does.
    #[test]
    fn run_started_is_emitted_on_the_wire() {
        let event = RunEvent::RunStarted {
            targets: vec![BeamId("build".to_string())],
            affected_by: None,
            base: None,
            beams: vec![BeamId("codegen".to_string()), BeamId("build".to_string())],
            edges: vec![(BeamId("build".to_string()), BeamId("codegen".to_string()))],
            details: Vec::new(),
        };

        let line = serde_json::to_string(&WireEvent::of(&event).unwrap()).unwrap();

        assert_eq!(
            line,
            r#"{"event":"run_started","targets":["build"],"beams":["codegen","build"],"edges":[["build","codegen"]]}"#
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

        let waiting = serde_json::to_string(&WireEvent::of(&waiting).unwrap()).unwrap();
        let triggered = serde_json::to_string(&WireEvent::of(&triggered).unwrap()).unwrap();

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
            report: None,
        };

        let line = serde_json::to_string(&WireEvent::of(&event).unwrap()).unwrap();

        assert_eq!(
            line,
            r#"{"event":"project_broken","diagnostic":"error: oh no\n"}"#
        );
    }

    /// A warning is one line of the stream, and only there: stderr keeps
    /// carrying nothing but broken-project diagnostics.
    #[test]
    fn a_warning_is_an_event_of_its_own() {
        let event = RunEvent::Warning {
            message: "cannot diff from `origin/nope`: no merge base; running in full mode"
                .to_string(),
        };
        assert_eq!(
            json(&event),
            serde_json::json!({
                "event": "warning",
                "message": "cannot diff from `origin/nope`: no merge base; running in full mode",
            })
        );
    }
}
