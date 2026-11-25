//! Snapshot tests for `alba_tui::ui::draw`: the header, progress row,
//! tree pane, log pane, and key hints as they render for a handful of
//! representative states. `TestBackend::to_string()` drops styles, so
//! these pin layout and text, not colour: see the spec's Layout
//! section for what each region is supposed to show.

use std::time::{Duration, Instant};

use alba_core::BeamId;
use alba_engine::{BeamStatus, RunEvent, RunSummary};
use alba_executors::{OutputLine, Stream};
use alba_tui::copy::CopyState;
use alba_tui::state::{AppState, Mode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            targets: vec![id("build")],
            affected_by: None,
            beams: ["codegen", "api:codegen", "api:build", "build", "test"]
                .iter()
                .map(|name| id(name))
                .collect(),
            edges: vec![(id("build"), id("codegen"))],
            details: Vec::new(),
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
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: vec![],
            details: Vec::new(),
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
            report: None,
        },
        Instant::now(),
    );
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// Below the floor, only the too-small screen renders.
#[test]
fn a_tiny_terminal_gets_the_too_small_screen() {
    let state = AppState::new("build", false);
    insta::assert_snapshot!(drawn(&state, 30, 8));
}

/// The real frame at the declared floor, a run in progress: the
/// narrowest width the layout still has to draw every band and both
/// panes on, rather than the too-small screen.
#[test]
fn a_run_in_flight_renders_at_the_forty_column_floor() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("codegen"), id("build")],
            edges: vec![(id("build"), id("codegen"))],
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
    state.apply(
        &RunEvent::BeamFinished {
            id: id("codegen"),
            status: BeamStatus::Succeeded,
            duration: Duration::from_millis(1200),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state.apply(&output("build", "Compiling api v0.1.0"), now);
    insta::assert_snapshot!(drawn(&state, 40, 12));
}

/// The floor's other exercise: a parked project, whose banner has to
/// give up its right half and whose header keeps the badge and the
/// state whole.
#[test]
fn a_parked_session_renders_at_the_forty_column_floor() {
    let mut state = AppState::new("build", true);
    state.apply(
        &RunEvent::ProjectBroken {
            diagnostic: "error: unknown target `nope`\n".to_string(),
            report: None,
        },
        Instant::now(),
    );
    insta::assert_snapshot!(drawn(&state, 40, 12));
}

/// Search mode: the key hints row shows the query and the match counter,
/// and the pane has scrolled so the current match (the earliest one, of
/// two) is on screen even though it sits well above the tail.
#[test]
fn a_search_highlights_and_counts_matches() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: vec![],
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    for index in 0..10 {
        state.apply(&output("build", &format!("line {index}")), now);
    }
    state.apply(&output("build", "ERROR one"), now);
    for index in 10..24 {
        state.apply(&output("build", &format!("line {index}")), now);
    }
    state.apply(&output("build", "ERROR two"), now);
    for index in 24..28 {
        state.apply(&output("build", &format!("line {index}")), now);
    }

    state.enter_search();
    for character in "error".chars() {
        state.handle_modal_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }

    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// Once `Enter` commits the query, the session is back in `Mode::Normal`
/// — a distinct state the user spends real time in — but the highlights
/// and the scroll position it left behind stay exactly as they were:
/// `last_search` is what keeps them alive, and the key hints go back
/// to Normal mode's own (offering `G follow`, since the search left the
/// pane scrolled away from the end).
#[test]
fn a_committed_search_keeps_its_highlights_in_normal_mode() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: vec![],
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    for index in 0..10 {
        state.apply(&output("build", &format!("line {index}")), now);
    }
    state.apply(&output("build", "ERROR one"), now);
    for index in 10..24 {
        state.apply(&output("build", &format!("line {index}")), now);
    }
    state.apply(&output("build", "ERROR two"), now);
    for index in 24..28 {
        state.apply(&output("build", &format!("line {index}")), now);
    }

    state.enter_search();
    for character in "error".chars() {
        state.handle_modal_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// Copy mode's selection renders as part of the frame. `TestBackend`
/// drops styles, so this pins layout and the key hints' own keymap
/// text — the reversed span itself is pinned by the unit test
/// `output::draw_marks_only_the_rows_the_selection_actually_covers`
/// instead (`TestBackend::to_string()` cannot see it either way).
#[test]
fn a_copy_selection_highlights_the_span() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: vec![],
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state.apply(&output("build", "alpha"), now);
    state.apply(&output("build", "bravo"), now);

    let mut copy = CopyState::new_at(0);
    copy.cursor = (1, 2);
    state.mode = Mode::Copy(copy);

    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// The layering tests' own three-beam chain (`build` depends on
/// `codegen`, `test` depends on `build`), with a different status at
/// each layer so the glyphs are worth reading in the snapshot.
fn three_beam_chain() -> AppState {
    let mut state = AppState::new("test", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("test")],
            affected_by: None,
            beams: vec![id("codegen"), id("build"), id("test")],
            edges: vec![(id("build"), id("codegen")), (id("test"), id("build"))],
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
    state.apply(
        &RunEvent::BeamFinished {
            id: id("codegen"),
            status: BeamStatus::Succeeded,
            duration: Duration::from_millis(900),
        },
        now,
    );
    // "build" stays running and "test" stays pending: three different
    // glyphs (✔, ▶, ○), one per layer.
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state
}

/// Graph mode replaces the whole body with the run's DAG: `codegen` at
/// the top (no dependencies), `build` beneath it, `test` at the bottom —
/// each linked to the layer above by a straight connector, since every
/// layer here holds exactly one node and so lines up in the same column.
#[test]
fn the_graph_view_draws_layers_and_edges() {
    let mut state = three_beam_chain();
    state.select(1); // "build"
    state.enter_graph();

    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// The help overlay: a centred bordered box over the dimmed body,
/// listing every binding the interactive modes actually implement (see
/// `ui/help.rs`'s own `KEYMAP` for the source of truth this is checked
/// against). `TestBackend` drops styles, so this pins the overlay's
/// text and its placement, not the dimming itself.
#[test]
fn the_help_overlay_lists_the_full_keymap() {
    let mut state = AppState::new("build", false);
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: vec![],
            details: Vec::new(),
        },
        Instant::now(),
    );
    state.enter_help();

    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// Arrow keys move the graph's focus (`GraphState::navigate`, reached
/// through the same modal-key path every other mode uses); two `Down`
/// presses from `codegen` (layer 0) cross to `build` and then to `test`.
/// The focused node's reversed style is invisible to
/// `TestBackend::to_string()` (see `ui/graphpane.rs`'s own
/// `node_style_reverses_only_the_focused_beam` unit test for that), so
/// this asserts directly on `GraphState::focused` to confirm navigation
/// actually landed where it should, and snapshots the resulting screen to
/// pin that the layout survives a real key-driven navigation, not just a
/// direct `GraphState::navigate` call.
#[test]
fn the_graph_view_focus_follows_navigation() {
    let mut state = three_beam_chain();
    state.select(0); // "codegen"
    state.enter_graph();

    state.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    state.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

    match &state.mode {
        Mode::Graph(graph) => assert_eq!(
            graph.focused, 2,
            "two Downs from codegen (layer 0) land on test (layer 2)"
        ),
        other => panic!("expected Mode::Graph, got {other:?}"),
    }
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// A line wider than the log pane wraps rather than being cut.
#[test]
fn a_long_log_line_wraps_in_the_pane() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: Vec::new(),
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state.apply(&output("build", &"x".repeat(60)), now);
    state.apply(&output("build", "tail"), now);
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// A long beam id never pushes the duration out of its column.
#[test]
fn a_long_beam_name_is_truncated_in_the_tree() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("services:payment:integration"), id("build")],
            edges: Vec::new(),
            details: Vec::new(),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamStarted {
            id: id("services:payment:integration"),
        },
        now,
    );
    state.apply(
        &RunEvent::BeamFinished {
            id: id("services:payment:integration"),
            status: BeamStatus::Succeeded,
            duration: Duration::from_millis(1200),
        },
        now,
    );
    insta::assert_snapshot!(drawn(&state, 80, 24));
}

/// `drawn` flattens styles away, so the buffer itself is read: in
/// monochrome a pending beam's row is dim (muted text), a running one is
/// not.
#[test]
fn a_pending_beam_is_dim_in_monochrome() {
    use ratatui::style::Modifier;
    let state = three_beam_chain();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| alba_tui::ui::draw(frame, &state, Instant::now()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let dim_of = |glyph: &str| {
        buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == glyph)
            .unwrap_or_else(|| panic!("no {glyph} drawn"))
            .modifier
            .contains(Modifier::DIM)
    };
    assert!(dim_of("○"), "pending glyph is dim");
    assert!(!dim_of("▶"), "running glyph is not dim");
}

/// The stream filter is applied by the drawing itself: hidden lines are
/// absent, Alba's own lines stay.
#[test]
fn a_stream_filter_hides_the_other_stream_in_the_drawing() {
    let mut state = AppState::new("build", false);
    let now = Instant::now();
    state.apply(
        &RunEvent::RunStarted {
            targets: vec![id("build")],
            affected_by: None,
            beams: vec![id("build")],
            edges: vec![],
            details: Vec::new(),
        },
        now,
    );
    state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
    state.apply(
        &RunEvent::CommandStarted {
            id: id("build"),
            command: "make".to_string(),
        },
        now,
    );
    state.apply(&output("build", "from stdout"), now);
    state.apply(
        &RunEvent::BeamOutput {
            id: id("build"),
            line: OutputLine {
                stream: Stream::Stderr,
                text: "from stderr".to_string(),
            },
            replayed: false,
        },
        now,
    );
    state.cycle_stream_filter(); // stdout only
    let screen = drawn(&state, 80, 24);
    assert!(screen.contains("from stdout"));
    assert!(screen.contains("$ make"));
    assert!(!screen.contains("from stderr"));
    state.cycle_stream_filter(); // stderr only
    let screen = drawn(&state, 80, 24);
    assert!(screen.contains("from stderr"));
    assert!(!screen.contains("from stdout"));
}

/// The parked diagnostic is not process output: no filter hides it.
#[test]
fn the_parked_diagnostic_stays_visible_under_a_stream_filter() {
    let mut state = AppState::new("build", false);
    state.apply(
        &RunEvent::ProjectBroken {
            diagnostic: "error: unknown target `nope`".to_string(),
            report: None,
        },
        Instant::now(),
    );
    state.cycle_stream_filter();
    state.cycle_stream_filter(); // stderr only
    assert!(drawn(&state, 80, 24).contains("unknown target"));
}
