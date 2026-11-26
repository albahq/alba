//! Snapshot tests for `alba_tui::ui::draw`: the header, progress row,
//! beams pane, output pane and key hints as they render. One snapshot
//! per artboard of the Paper file "Alba", page "TUI v2", built from the
//! artboard's own beams, outputs and durations, then the states the
//! artboards leave out: the 40x12 floor, a cancelled run, a plain
//! diagnostic, a committed search, wrapping and truncation.
//! `TestBackend::to_string()` drops styles, so these pin layout and
//! text, not colour.

use std::time::{Duration, Instant};

use alba_core::BeamId;
use alba_engine::{BeamDetails, BeamStatus, ExecutorLabel, RunEvent, RunSummary};
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

/// Once `Enter` commits the query, the session is back in `Mode::Normal`
/// — a distinct state the user spends real time in — but the highlights
/// and the scroll position it left behind stay exactly as they were:
/// `last_search` is what keeps them alive, and the key hints go back
/// to Normal mode's own (offering `G follow`, since the search left the
/// pane scrolled away from the end). The pane is sized as `dispatch`
/// sizes it, so each keystroke scrolls only as far as its match needs:
/// `e` already matches `line 0` and takes the pane to the top, where
/// `ERROR one` is in view.
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

    sized(&mut state, 80, 24);
    state.enter_search();
    for character in "error".chars() {
        state.handle_modal_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

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

/// At 80 columns the two columns do not fit: the groups stack in one,
/// cut at the bottom.
#[test]
fn a_narrow_help_page_stacks_its_groups() {
    insta::assert_snapshot!(drawn(&helping(), 80, 24));
}

fn helping() -> AppState {
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
    state
}

/// Arrow keys move the graph's focus (`GraphState::navigate`, reached
/// through the same modal-key path every other mode uses); two `Down`
/// presses from `codegen` (layer 0) cross to `build` and then to `test`.
/// The focused node's style is invisible to `TestBackend::to_string()`
/// (see `ui/graphpane.rs`'s own
/// `nodes_take_their_status_colour_and_the_focused_one_is_the_bar` unit
/// test for that), so
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

// ---------------------------------------------------------------------
// One snapshot per artboard of the Paper file "Alba", page "TUI v2":
// each state is built from the beams, outputs and durations the
// artboard shows, at its 104x23 grid.
// ---------------------------------------------------------------------

const ARTBOARD_WIDTH: u16 = 104;
const ARTBOARD_HEIGHT: u16 = 23;

fn millis(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn drawn_at(state: &AppState, width: u16, height: u16, now: Instant) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| alba_tui::ui::draw(frame, state, now))
        .unwrap();
    terminal.backend().to_string()
}

/// What `lib::dispatch` does before every action: tell the state the
/// output pane's size in a `width` x `height` terminal, which search
/// and copy scroll against.
fn sized(state: &mut AppState, width: u16, height: u16) {
    let pane = alba_tui::ui::log_pane_content_area(state, width, height)
        .expect("the terminal is above the size floor");
    state.set_pane_size(pane.height as usize, pane.width as usize);
}

fn sized_for_artboard(state: &mut AppState) {
    sized(state, ARTBOARD_WIDTH, ARTBOARD_HEIGHT);
}

fn artboard(state: &AppState, now: Instant) -> String {
    drawn_at(state, ARTBOARD_WIDTH, ARTBOARD_HEIGHT, now)
}

fn shell() -> ExecutorLabel {
    ExecutorLabel {
        kind: "shell".to_string(),
        detail: Some("(embedded)".to_string()),
    }
}

fn detail(beam: &str, description: &str, location: &str, commands: &[&str]) -> BeamDetails {
    BeamDetails {
        id: id(beam),
        description: Some(description.to_string()),
        location: Some(location.to_string()),
        executor: shell(),
        commands: commands.iter().map(|command| command.to_string()).collect(),
    }
}

fn started(
    beams: &[&str],
    edges: &[(&str, &str)],
    target: &str,
    details: Vec<BeamDetails>,
) -> RunEvent {
    RunEvent::RunStarted {
        targets: vec![id(target)],
        affected_by: None,
        beams: beams.iter().map(|name| id(name)).collect(),
        edges: edges.iter().map(|(from, to)| (id(from), id(to))).collect(),
        details,
    }
}

fn finished(beam: &str, status: BeamStatus, duration: Duration) -> RunEvent {
    RunEvent::BeamFinished {
        id: id(beam),
        status,
        duration,
    }
}

fn command(beam: &str, text: &str) -> RunEvent {
    RunEvent::CommandStarted {
        id: id(beam),
        command: text.to_string(),
    }
}

fn stderr(beam: &str, text: &str) -> RunEvent {
    RunEvent::BeamOutput {
        id: id(beam),
        line: OutputLine {
            stream: Stream::Stderr,
            text: text.to_string(),
        },
        replayed: false,
    }
}

/// The `check` project of the artboards: `fmt`, `lint` and `test`,
/// all needed by `check`.
const CHECK_BEAMS: [&str; 4] = ["fmt", "lint", "test", "check"];
const CHECK_EDGES: [(&str, &str); 3] = [("check", "fmt"), ("check", "lint"), ("check", "test")];

fn check_details() -> Vec<BeamDetails> {
    vec![
        detail(
            "fmt",
            "Check formatting",
            "Beamfile:8",
            &["cargo fmt --all --check"],
        ),
        detail(
            "lint",
            "Lint every crate",
            "Beamfile:15",
            &["cargo clippy --workspace -- -D warnings"],
        ),
        detail(
            "test",
            "Run the test suite",
            "Beamfile:23",
            &["cargo build --workspace", "cargo test --workspace"],
        ),
        detail(
            "check",
            "Full verification: format, lints, tests",
            "Beamfile:3",
            &["echo all checks passed"],
        ),
    ]
}

/// `fmt` passed in 0.8s and `lint` came from the cache in 0.1s.
fn check_run_started(state: &mut AppState, start: Instant) {
    state.apply(
        &started(&CHECK_BEAMS, &CHECK_EDGES, "check", check_details()),
        start,
    );
    state.apply(&RunEvent::BeamStarted { id: id("fmt") }, start);
    state.apply(&command("fmt", "cargo fmt --all --check"), start);
    state.apply(
        &finished("fmt", BeamStatus::Succeeded, millis(800)),
        start + millis(800),
    );
    state.apply(&RunEvent::BeamCached { id: id("lint") }, start);
    state.apply(
        &finished("lint", BeamStatus::Cached, millis(100)),
        start + millis(100),
    );
}

fn test_output_head(state: &mut AppState, at: Instant) {
    state.apply(&command("test", "cargo test --workspace"), at);
    state.apply(&stderr("test", "   Compiling alba-core v0.6.0"), at);
    state.apply(&stderr("test", "   Compiling alba-tui v0.6.0"), at);
    state.apply(&stderr("test", "    Finished `test` profile in 2.46s"), at);
    state.apply(&stderr("test", "     Running unittests src/lib.rs"), at);
    state.apply(&output("test", ""), at);
    state.apply(&output("test", "running 67 tests"), at);
    for name in [
        "unknown_need_suggests_closest",
        "cycle_reports_exact_path",
        "subgraph_is_transitive_closure",
        "self_cycle_reports_itself",
    ] {
        state.apply(
            &output("test", &format!("test graph::tests::{name} ... ok")),
            at,
        );
    }
}

/// Run · Live output: `test` running for 8.1s, its output followed.
#[test]
fn run_live_output() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_started(&mut state, start);
    let test_start = start + millis(300);
    state.apply(&RunEvent::BeamStarted { id: id("test") }, test_start);
    test_output_head(&mut state, test_start);
    state.select(2);
    insta::assert_snapshot!(artboard(&state, start + millis(8400)));
}

/// Graph · Dependencies: the live run's DAG, `test` focused.
#[test]
fn graph_dependencies() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_started(&mut state, start);
    let test_start = start + millis(300);
    state.apply(&RunEvent::BeamStarted { id: id("test") }, test_start);
    test_output_head(&mut state, test_start);
    state.select(2);
    state.enter_graph();
    insta::assert_snapshot!(artboard(&state, start + millis(8400)));
}

/// `test` ran 8.5s and passed, then `check` echoed its verdict in 0.1s:
/// the whole run took 9.4s.
fn check_run_passed(state: &mut AppState, start: Instant, test_duration: Duration) {
    check_run_started(state, start);
    state.apply(&RunEvent::BeamStarted { id: id("test") }, start);
    state.apply(&command("test", "cargo test --workspace"), start);
    state.apply(
        &output("test", "test result: ok. 67 passed; 0 failed; 0 ignored"),
        start,
    );
    state.apply(
        &finished("test", BeamStatus::Succeeded, test_duration),
        start + test_duration,
    );
    state.apply(
        &RunEvent::BeamStarted { id: id("check") },
        start + test_duration,
    );
    state.apply(
        &command("check", "echo all checks passed"),
        start + test_duration,
    );
    state.apply(&output("check", "all checks passed"), start + test_duration);
    state.apply(
        &finished("check", BeamStatus::Succeeded, millis(100)),
        start + test_duration + millis(100),
    );
    state.apply(
        &RunEvent::RunFinished {
            summary: RunSummary {
                succeeded: vec![id("fmt"), id("test"), id("check")],
                cached: vec![id("lint")],
                duration: millis(9400),
                ..RunSummary::default()
            },
        },
        start + millis(9400),
    );
}

/// Run · Passed: every beam passed or cached, `check` selected.
#[test]
fn run_passed() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_passed(&mut state, start, millis(8500));
    state.select(3);
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// Watch · Waiting for changes: the passed run, then the wait on three
/// watched files.
#[test]
fn watch_waiting_for_changes() {
    let start = Instant::now();
    let mut state = AppState::new("check", true);
    check_run_passed(&mut state, start, millis(8500));
    state.apply(&RunEvent::WatchWaiting { files: 3 }, start + millis(9400));
    state.select(3);
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// Watch · Parked on diagnostic: the Beamfile now needs `tset`, which
/// does not exist; the last run's beams stay on the left.
#[test]
fn watch_parked_on_diagnostic() {
    let start = Instant::now();
    let mut state = AppState::new("check", true);
    check_run_passed(&mut state, start, millis(8900));
    state.apply(&RunEvent::WatchWaiting { files: 3 }, start + millis(9400));
    let snippet = |number: usize, text: &str, faulty: bool| alba_core::SnippetLine {
        number,
        text: text.to_string(),
        faulty,
    };
    state.apply(
        &RunEvent::ProjectBroken {
            diagnostic: "error: unknown beam `tset`\n".to_string(),
            report: Some(alba_core::DiagnosticReport {
                message: "unknown beam `tset`".to_string(),
                path: "Beamfile".to_string(),
                line: 5,
                column: 21,
                snippet: vec![
                    snippet(3, "beam check {", false),
                    snippet(
                        4,
                        "  description \"Full verification: format, lints, tests\"",
                        false,
                    ),
                    snippet(5, "  needs [fmt, lint, tset]", true),
                    snippet(6, "  run \"echo all checks passed\"", false),
                    snippet(7, "}", false),
                ],
                caret: 20..24,
                help: Some("did you mean `test`?".to_string()),
            }),
        },
        start + millis(15_000),
    );
    state.select(2);
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// `test` failed with exit 101 after 4.2s, so `check` was skipped; the
/// run took 12.3s.
fn check_run_failed(state: &mut AppState, start: Instant) {
    check_run_started(state, start);
    state.apply(&RunEvent::BeamStarted { id: id("test") }, start);
    state.apply(&command("test", "cargo test --workspace"), start);
    state.apply(&stderr("test", "     Running unittests src/lib.rs"), start);
    state.apply(&output("test", "running 67 tests"), start);
    state.apply(
        &output(
            "test",
            "test graph::tests::cycle_reports_exact_path ... FAILED",
        ),
        start,
    );
    state.apply(
        &stderr(
            "test",
            "thread 'graph::tests::cycle_reports_exact_path' panicked at src/graph.rs:142:9:",
        ),
        start,
    );
    state.apply(&stderr("test", "assertion `left == right` failed"), start);
    state.apply(&stderr("test", "  left: [\"a\", \"b\", \"a\"]"), start);
    state.apply(
        &stderr("test", " right: [\"a\", \"b\", \"c\", \"a\"]"),
        start,
    );
    state.apply(
        &output(
            "test",
            "test result: FAILED. 66 passed; 1 failed; 0 ignored",
        ),
        start,
    );
    state.apply(&output("test", ""), start);
    state.apply(
        &finished("test", BeamStatus::Failed { exit_code: 101 }, millis(4200)),
        start + millis(4200),
    );
    state.apply(
        &finished("check", BeamStatus::Cancelled, Duration::ZERO),
        start + millis(4200),
    );
    state.apply(
        &RunEvent::RunFinished {
            summary: RunSummary {
                succeeded: vec![id("fmt")],
                cached: vec![id("lint")],
                failed: vec![id("test")],
                cancelled: vec![id("check")],
                duration: millis(12_300),
                ..RunSummary::default()
            },
        },
        start + millis(12_300),
    );
}

/// Run · Failed: the failure selected, its output at its end line.
#[test]
fn run_failed() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_failed(&mut state, start);
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// Search · Matches in output: `failed` typed against the failure.
#[test]
fn search_matches_in_output() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_failed(&mut state, start);
    sized_for_artboard(&mut state);
    state.enter_search();
    for character in "failed".chars() {
        state.handle_modal_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// Copy · Line selection: the panic message, from its first line to the
/// end of `right:`.
#[test]
fn copy_line_selection() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_failed(&mut state, start);
    // Buffer lines: 0 the echo, 1 `Running`, 2 `running 67 tests`,
    // 3 the failed test, 4 the panic, 5 the assertion, 6 `left`,
    // 7 `right`.
    sized_for_artboard(&mut state);
    let mut copy = CopyState::new_at(4);
    copy.cursor = (7, 28);
    state.mode = Mode::Copy(copy);
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// Output · stderr only: the failure filtered to stderr; the echo and
/// the end line are Alba's, so they stay.
#[test]
fn output_stderr_only() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_failed(&mut state, start);
    state.cycle_stream_filter();
    state.cycle_stream_filter();
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}

/// Help · Keyboard page, over the live run.
#[test]
fn help_keyboard_page() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_started(&mut state, start);
    let test_start = start + millis(300);
    state.apply(&RunEvent::BeamStarted { id: id("test") }, test_start);
    test_output_head(&mut state, test_start);
    state.select(2);
    state.enter_help();
    insta::assert_snapshot!(artboard(&state, start + millis(8400)));
}

/// Terminal too small · 36x10.
#[test]
fn terminal_too_small() {
    let state = AppState::new("check", false);
    insta::assert_snapshot!(drawn_at(&state, 36, 10, Instant::now()));
}

/// The 48 beams of the `ci` run, in their order: fourteen before the
/// ones the artboard shows, its thirteen, then twenty-one after.
const CI_HEAD: [&str; 14] = [
    "codegen:proto",
    "codegen:schema",
    "infra:migrations",
    "infra:seed",
    "lib:core:build",
    "lib:core:test",
    "lib:http:build",
    "lib:http:test",
    "services:api:build",
    "services:api:lint",
    "services:api:test",
    "services:auth:build",
    "services:auth:test",
    "services:billing:build",
];
const CI_SHOWN: [&str; 13] = [
    "services:billing:lint",
    "services:billing:test",
    "services:catalog:build",
    "services:catalog:lint",
    "services:catalog:test",
    "services:identity:build",
    "services:identity:lint",
    "services:payment:integration",
    "services:payment:unit",
    "services:search:build",
    "services:search:lint",
    "services:search:test",
    "web:dashboard:build",
];
const CI_TAIL: [&str; 21] = [
    "web:dashboard:lint",
    "web:dashboard:test",
    "web:storefront:build",
    "web:storefront:lint",
    "web:storefront:test",
    "web:admin:build",
    "web:admin:lint",
    "web:admin:test",
    "e2e:checkout",
    "e2e:refund",
    "e2e:search",
    "docs:build",
    "docs:linkcheck",
    "docs:publish",
    "release:notes",
    "release:changelog",
    "release:package",
    "release:sign",
    "release:upload",
    "release:announce",
    "ci",
];

/// Run · Scrolling 48 beams: 22nd of 48 selected, its 2 400-line
/// output paused with line 1 240 at the bottom of the pane.
#[test]
fn run_scrolling_48_beams() {
    let start = Instant::now();
    let names: Vec<&str> = CI_HEAD
        .iter()
        .chain(&CI_SHOWN)
        .chain(&CI_TAIL)
        .copied()
        .collect();
    assert_eq!(names.len(), 48);
    let mut state = AppState::new("ci", false);
    let details = vec![BeamDetails {
        id: id("services:payment:integration"),
        description: Some("Payment sandbox flows".to_string()),
        location: Some("services/payment/Beamfile:12".to_string()),
        executor: shell(),
        commands: vec!["cargo test -p payment --test sandbox".to_string()],
    }];
    state.apply(&started(&names, &[], "ci", details), start);
    // The shown beams with the artboard's statuses, then enough of the
    // others for 23 passed, 7 cached and 1 failed: ten passed and four
    // cached before, nine passed and one cached after.
    let mut done: Vec<(&str, BeamStatus, u64)> = vec![
        ("services:billing:lint", BeamStatus::Succeeded, 3100),
        ("services:billing:test", BeamStatus::Succeeded, 41_000),
        ("services:catalog:build", BeamStatus::Cached, 200),
        ("services:catalog:lint", BeamStatus::Cached, 100),
        (
            "services:catalog:test",
            BeamStatus::Failed { exit_code: 1 },
            12_700,
        ),
        ("services:identity:build", BeamStatus::Succeeded, 18_400),
        ("services:identity:lint", BeamStatus::Succeeded, 2200),
    ];
    for (index, name) in CI_HEAD.iter().enumerate() {
        let status = if index < 10 {
            BeamStatus::Succeeded
        } else {
            BeamStatus::Cached
        };
        done.push((name, status, 1500));
    }
    for (index, name) in CI_TAIL[..10].iter().enumerate() {
        let status = if index < 9 {
            BeamStatus::Succeeded
        } else {
            BeamStatus::Cached
        };
        done.push((name, status, 1500));
    }
    for (name, status, duration) in done {
        if matches!(status, BeamStatus::Cached) {
            state.apply(&RunEvent::BeamCached { id: id(name) }, start);
        } else {
            state.apply(&RunEvent::BeamStarted { id: id(name) }, start);
        }
        state.apply(&finished(name, status, millis(duration)), start);
    }
    let now = start + millis(134_000);
    state.apply(
        &RunEvent::BeamStarted {
            id: id("services:payment:integration"),
        },
        now - millis(62_000),
    );
    state.apply(
        &RunEvent::BeamStarted {
            id: id("services:payment:unit"),
        },
        now - millis(24_900),
    );
    let visible = [
        "payment/checkout/session_expires_after_ttl",
        "payment/checkout/rejects_currency_mismatch",
        "payment/refund/partial_refund_keeps_ledger_balanced",
        "payment/refund/full_refund_closes_the_intent",
        "payment/refund/refund_after_dispute_is_rejected",
        "payment/webhook/replayed_event_is_idempotent",
        "payment/webhook/unsigned_event_is_rejected",
        "payment/webhook/out_of_order_events_converge",
        "payment/ledger/double_entry_balances_per_currency",
        "payment/ledger/rounding_follows_bankers_rule",
        "payment/payout/batch_skips_frozen_accounts",
        "payment/payout/retries_on_transient_bank",
    ];
    // The pane shows twelve lines ending on line 1 240 of 2 400.
    let first_visible = 1240 - visible.len();
    for index in 0..2400_usize {
        let name = match index.checked_sub(first_visible) {
            Some(offset) if offset < visible.len() => visible[offset].to_string(),
            _ => format!("payment/sandbox/case_{index:04}"),
        };
        state.apply(
            &output("services:payment:integration", &format!("ok   {name}")),
            now,
        );
    }
    state
        .logs
        .get_mut("services:payment:integration")
        .unwrap()
        .scroll_up(2400 - 1240);
    state.select(21);
    insta::assert_snapshot!(artboard(&state, now));
}

/// A run cancelled while `test` was running: `check` never started, so
/// it reads as skipped; the header says how long the run had lasted.
#[test]
fn run_cancelled() {
    let start = Instant::now();
    let mut state = AppState::new("check", false);
    check_run_started(&mut state, start);
    let test_start = start + millis(300);
    state.apply(&RunEvent::BeamStarted { id: id("test") }, test_start);
    test_output_head(&mut state, test_start);
    state.select(2);
    state.mark_user_cancelled();
    state.apply(
        &finished("test", BeamStatus::Cancelled, millis(2000)),
        test_start + millis(2000),
    );
    state.apply(
        &finished("check", BeamStatus::Cancelled, Duration::ZERO),
        test_start + millis(2000),
    );
    state.apply(
        &RunEvent::RunFinished {
            summary: RunSummary {
                succeeded: vec![id("fmt")],
                cached: vec![id("lint")],
                cancelled: vec![id("test"), id("check")],
                duration: millis(2300),
                ..RunSummary::default()
            },
        },
        start + millis(2300),
    );
    insta::assert_snapshot!(artboard(&state, start + millis(20_000)));
}
