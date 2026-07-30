//! Snapshot tests for `alba_tui::ui::draw`: the header, tree pane, log
//! pane, and bottom bar as they render for a handful of representative
//! states. `TestBackend::to_string()` drops styles, so these pin layout
//! and text, not colour — see the spec's Layout section for what each
//! region is supposed to show.

use std::time::{Duration, Instant};

use alba_core::BeamId;
use alba_engine::{BeamStatus, RunEvent, RunSummary};
use alba_executors::{OutputLine, Stream};
use alba_tui::state::AppState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn id(name: &str) -> BeamId {
    BeamId(name.to_string())
}

fn output(beam: &str, text: &str) -> RunEvent {
    RunEvent::BeamOutput {
        id: id(beam),
        line: OutputLine {
            stream: Stream::Stdout,
            text: text.to_string(),
        },
        replayed: false,
    }
}

fn drawn(state: &AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| alba_tui::ui::draw(frame, state, Instant::now()))
        .unwrap();
    terminal.backend().to_string()
}

/// The spec's main mockup: run in flight, one success, one cache hit,
/// one running, two pending.
#[test]
fn a_run_in_flight_renders_tree_bar_and_logs() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            target: id("build"),
            beams: ["codegen", "api:codegen", "api:build", "build", "test"]
                .iter()
                .map(|name| id(name))
                .collect(),
            edges: vec![(id("build"), id("codegen"))],
        },
        now,
    );
    // codegen succeeded, api:codegen cached, api:build running with output
    state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
    state.apply(
        &RunEvent::BeamFinished {
            id: id("codegen"),
            status: BeamStatus::Succeeded,
            duration: Duration::from_millis(1200),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamCached {
            id: id("api:codegen"),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamFinished {
            id: id("api:codegen"),
            status: BeamStatus::Cached,
            duration: Duration::from_millis(800),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamStarted {
            id: id("api:build"),
        },
        now,
    );
    state.apply(&output("api:build", "Compiling api v0.1.0"), now);
    state.select(2); // api:build
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// A failure keeps the TUI open and marks the row.
#[test]
fn a_failed_run_renders_the_cross_and_summary_counts() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            target: id("build"),
            beams: vec![id("build")],
            edges: vec![],
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state.apply(
        &RunEvent::BeamFinished {
            id: id("build"),
            status: BeamStatus::Failed { exit_code: 2 },
            duration: Duration::from_millis(500),
        },
        now,
    );
    state.apply(
        &RunEvent::RunFinished {
            summary: RunSummary {
                failed: vec![id("build")],
                duration: Duration::from_millis(500),
                ..RunSummary::default()
            },
        },
        now,
    );
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// The watch idle state: waiting header, no bar.
#[test]
fn a_waiting_session_renders_the_watch_state() {
    let mut state = AppState::new("build", true);
    state.apply(&RunEvent::WatchWaiting { files: 42 }, Instant::now());
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// A parked session shows the diagnostic in the log pane.
#[test]
fn a_parked_session_renders_the_diagnostic() {
    let mut state = AppState::new("build", true);
    state.apply(
        &RunEvent::ProjectBroken {
            diagnostic: "error: unknown target `nope`\n".to_string(),
        },
        Instant::now(),
    );
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// Below the floor, only the message renders.
#[test]
fn a_tiny_terminal_gets_the_too_small_screen() {
    let state = AppState::new("build", false);
    insta::assert_snapshot!(drawn(&state, 30, 8));
}
