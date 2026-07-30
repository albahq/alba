//! The interactive terminal interface: a consumer of the engine's
//! [`alba_engine::RunEvent`] stream and a producer of
//! [`alba_engine::SessionCommand`]s — the interactive mirror of the CLI's
//! headless renderers. Knows nothing about how a beam executes.
//!
//! [`run`] is where the pure pieces meet the terminal: it folds events
//! into [`state::AppState`], turns keys into intents through
//! [`input::action_for`], and resolves those intents — in [`dispatch`] —
//! into a state mutation, a command to the session, or both. Everything
//! it does not do itself lives one layer down and is tested there.

pub mod copy;
pub mod input;
pub mod logs;
pub mod search;
pub mod state;
pub mod terminal;
pub mod ui;

use std::io;
use std::time::{Duration, Instant};

use alba_core::BeamId;
use alba_engine::{RunEvent, RunSummary, SessionCommand};
use crossterm::event::{MouseEvent, MouseEventKind};
use futures::StreamExt;
use ratatui::layout::Size;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::input::Action;
use crate::logs::LogBuffer;
use crate::state::AppState;

/// The heartbeat between events. Redrawing is event-driven, but elapsed
/// times and the progress bar have to keep moving while nothing else
/// arrives, and a copy confirmation has its own deadline to reach — so
/// the loop wakes on its own this often and redraws whenever
/// `tick_should_redraw` says one of those is still ticking on its own;
/// otherwise an idle screen shows nothing that a clock changes.
const TICK: Duration = Duration::from_millis(80);

/// How many queued events one frame may absorb. Coalescing a burst into
/// a single redraw is the point; draining an endlessly refilled queue is
/// not, since nothing else — not a keystroke, not the tick — is polled
/// until the batch ends.
const MAX_BATCH: usize = 256;

pub struct TuiOptions {
    pub target: String,
    pub watch: bool,
}

#[derive(Debug)]
pub struct TuiOutcome {
    /// `Some(code)` iff the last run ran to completion un-abandoned.
    pub last_run_code: Option<i32>,
    pub last_summary: Option<RunSummary>,
    /// `(beam id, its buffered lines)` for the last summary's failed
    /// beams, for the CLI's exit replay.
    pub failed_logs: Vec<(String, Vec<String>)>,
}

/// Drives the interface until the user quits or the session ends.
///
/// The terminal is held by a guard for the whole call, so every exit
/// path — including a failed draw — restores it on the way out.
pub async fn run(
    mut events: UnboundedReceiver<RunEvent>,
    commands: UnboundedSender<SessionCommand>,
    options: TuiOptions,
) -> io::Result<TuiOutcome> {
    let mut guard = terminal::TerminalGuard::enter()?;
    let mut state = AppState::new(&options.target, options.watch);
    let mut input = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    // A loop that fell behind owes the user one fresh frame, not a burst
    // of catch-up ticks that all draw the same thing.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;

    while !state.should_quit {
        if dirty {
            let now = Instant::now();
            guard
                .terminal()
                .draw(|frame| ui::draw(frame, &state, now))?;
            dirty = false;
            // Once a draw has actually shown a copy confirmation past
            // its own window, there is nothing left for the tick to
            // force further redraws for — see `tick_should_redraw`.
            clear_expired_copy_result(&mut state, now);
        }

        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    let now = Instant::now();
                    state.apply(&event, now);
                    // Whatever is already queued behind it folds into the
                    // same frame: a beam flooding its output must cost one
                    // redraw, not one per line. Bounded, because a beam
                    // producing faster than this loop drains would
                    // otherwise keep the batch non-empty forever and
                    // starve both the keyboard and the tick.
                    for _ in 0..MAX_BATCH {
                        match events.try_recv() {
                            Ok(event) => state.apply(&event, now),
                            Err(_) => break,
                        }
                    }
                    dirty = true;
                }
                // The session ended on its own (its error path); there is
                // nothing left to drive, so leave cleanly.
                None => break,
            },
            input_event = input.next() => match input_event {
                Some(Ok(event)) => {
                    let action = input::action_for(&event, &state.mode);
                    // Best effort: an error querying the terminal's own
                    // size falls back to zero, which reads as "too small"
                    // to `ui::log_pane_content_area` (no pane to click or
                    // drag into) and clamps `enter_copy`'s anchor to the
                    // buffer's own last line rather than one past it —
                    // never a panic either way.
                    let terminal_size = guard.terminal().size().unwrap_or_default();
                    dispatch(&mut state, &commands, action, terminal_size);
                    // The one place `AppState`'s copy intent turns into
                    // the real clipboard side effect — kept out of
                    // `dispatch` (and so out of `AppState`, which never
                    // touches the terminal or a clipboard daemon itself)
                    // and here at the composition root instead.
                    if let Some(text) = state.pending_copy.take() {
                        let message = copy::copy_to_clipboard(&text);
                        state.record_copy_result(message, Instant::now());
                    }
                    dirty = true;
                }
                // A terminal that can no longer be read cannot be driven
                // *or* escaped: raw mode routes Ctrl-C through this very
                // stream, so a TUI that keeps drawing past its input is
                // one the user has no way out of. Both the end of the
                // stream and a failed read end the session instead —
                // interrupted, not concluded, so no run vouches for it.
                Some(Err(_)) | None => state.quit_via_interrupt(),
            },
            _ = tick.tick() => {
                dirty = tick_should_redraw(&state);
            },
        }
    }

    let _ = commands.send(SessionCommand::Shutdown);
    // Drain until the channel closes so the session's final events (the
    // cancelled run's summary) still reach the state and the replay.
    while let Some(event) = events.recv().await {
        state.apply(&event, Instant::now());
    }
    Ok(outcome(&state))
}

/// Whether the tick alone — no event, no keystroke — should force a
/// redraw. A run in flight has the progress bar and elapsed time
/// ticking on their own; a copy confirmation has its own two-second
/// deadline to reach (`CopyResult::is_visible`), and copy mode's most
/// common use is reviewing a *finished* run's output or copying the
/// diagnostic while parked — `Finished`, `Waiting`, and `Parked` are
/// all `running() == false`. Without this, an idle session would leave
/// the confirmation on screen until an unrelated keypress or event
/// happened to redraw it away instead of on its own schedule.
fn tick_should_redraw(state: &AppState) -> bool {
    state.running() || state.last_copy_result.is_some()
}

/// Once a draw has actually shown a copy confirmation past its own
/// window, `tick_should_redraw` has nothing left to force further
/// redraws for — clearing it here (right after the draw that painted
/// the now-expired state) is what lets the tick stop waking the loop up
/// for it.
fn clear_expired_copy_result(state: &mut AppState, now: Instant) {
    if let Some(result) = &state.last_copy_result
        && !result.is_visible(now)
    {
        state.last_copy_result = None;
    }
}

/// The one place a user intent meets the state and the session.
///
/// Kept apart from the loop — and free of any terminal of its own — so
/// the rules the exit code depends on can be tested by handing it an
/// action and reading back what it did. `terminal_size` is the one bit
/// of screen geometry actions need (`EnterCopy`'s anchor, keyboard
/// scrolling in copy mode, `Mouse`'s hit test): a plain size value
/// rather than a live terminal, so this stays just as testable as
/// everything else here.
///
/// Never attempts the clipboard write copy mode's `y` or a mouse release
/// stages in `AppState::pending_copy` — that stays the caller's job
/// (`lib::run`'s loop), so a test that presses `y` through `dispatch`
/// alone can assert on `pending_copy` without ever reaching a real
/// terminal or clipboard.
fn dispatch(
    state: &mut AppState,
    commands: &UnboundedSender<SessionCommand>,
    action: Action,
    terminal_size: Size,
) {
    // Refreshed before every action: `EnterCopy`'s anchor and copy
    // mode's keyboard-driven scroll-follow both need to know the log
    // pane's current content height, and this is the one place that
    // reaches the terminal size to compute it.
    state.set_pane_height(log_pane_height(terminal_size));
    match action {
        Action::Quit => {
            // Quitting on a run in flight abandons it: its summary,
            // whatever it ends up saying, is not a verdict on the
            // sources. Outside a run the call is inert (`AppState`
            // marks nothing when nothing is running), which is what
            // leaves a finished run's code standing.
            state.mark_user_cancelled();
            state.should_quit = true;
        }
        Action::CancelOrQuit => {
            if state.running() {
                state.mark_user_cancelled();
                let _ = commands.send(SessionCommand::CancelRun);
            } else {
                state.quit_via_interrupt();
            }
        }
        Action::Rerun { force } => {
            if let Some(beam) = state.selected_beam() {
                let id = BeamId(beam.id.clone());
                // The session cancels the run in flight for us; the state
                // has to know the superseded run may not vouch. Inert
                // when no run is in flight, so re-running from an idle
                // session does not disown the run that just finished.
                state.mark_user_cancelled();
                let _ = commands.send(SessionCommand::RunBeam { id, force });
            }
        }
        Action::CancelRun => {
            if state.running() {
                state.mark_user_cancelled();
                let _ = commands.send(SessionCommand::CancelRun);
            }
        }
        Action::ToggleWatch => {
            state.watch_enabled = !state.watch_enabled;
            let _ = commands.send(SessionCommand::SetWatch(state.watch_enabled));
        }
        Action::SelectNext => state.select_next(),
        Action::SelectPrevious => state.select_previous(),
        Action::ScrollUp(lines) => {
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.scroll_up(lines);
            }
        }
        Action::ScrollDown(lines) => {
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.scroll_down(lines);
            }
        }
        Action::FollowTail => {
            if let Some(buffer) = selected_buffer_mut(state) {
                buffer.follow_tail();
            }
        }
        Action::EnterSearch => state.enter_search(),
        Action::SearchNext => state.search_next(),
        Action::SearchPrevious => state.search_previous(),
        Action::EnterCopy => state.enter_copy(),
        // Graph and Help have no renderer yet. Entering a mode nothing
        // draws leaves the user facing an unchanged screen whose bottom
        // bar advertises `q quit` while, modal, `q` is swallowed as a
        // plain character — a false affordance. Until each mode's own
        // task lands, its key does nothing at all.
        Action::EnterGraph | Action::EnterHelp => {}
        Action::LeaveMode => state.leave_mode(),
        Action::Key(key) => state.handle_modal_key(key),
        Action::Mouse(mouse_event) => dispatch_mouse(state, mouse_event, terminal_size),
        Action::None => {}
    }
}

/// Copy mode's own hit testing: a click or a drag only means something
/// once it lands inside the log pane's own content area — the tree
/// pane, the borders, and the title/footer rows are not part of the
/// buffer copy mode addresses. A release (`Up`) is the one exception:
/// it always finishes whatever selection is already there, wherever the
/// pointer ended up, since dragging off the bottom of the buffer or
/// into the tree pane and letting go there are both routine and must
/// not strand the user mid-selection with nothing copied. Every other
/// mode leaves a mouse event exactly as inert as before this task;
/// their own hit testing (e.g. click-to-select in the tree) is a later
/// task's to add.
fn dispatch_mouse(state: &mut AppState, mouse: MouseEvent, terminal_size: Size) {
    if matches!(mouse.kind, MouseEventKind::Up(_)) {
        state.finish_copy();
        return;
    }
    let Some(area) = ui::log_pane_content_area(terminal_size.width, terminal_size.height) else {
        return;
    };
    if mouse.row < area.y
        || mouse.row >= area.y + area.height
        || mouse.column < area.x
        || mouse.column >= area.x + area.width
    {
        return;
    }
    let pane_row = (mouse.row - area.y) as usize;
    let pane_col = (mouse.column - area.x) as usize;
    state.handle_mouse(mouse.kind, area.height as usize, pane_row, pane_col);
}

fn log_pane_height(terminal_size: Size) -> usize {
    ui::log_pane_content_area(terminal_size.width, terminal_size.height)
        .map(|area| area.height as usize)
        .unwrap_or(0)
}

/// The selected beam's buffer, created on demand — scrolling a beam that
/// has printed nothing yet is still a legitimate thing to do. `None`
/// only when no beam is selected at all (before the first run).
fn selected_buffer_mut(state: &mut AppState) -> Option<&mut LogBuffer> {
    let id = state.selected_beam()?.id.clone();
    Some(state.logs.entry(id).or_default())
}

/// What the CLI needs once the screen is gone: the exit code the session
/// earned, and enough of the log to replay the failures on stderr.
fn outcome(state: &AppState) -> TuiOutcome {
    let failed_logs = state
        .last_summary
        .iter()
        .flat_map(|summary| &summary.failed)
        .map(|id| (id.0.clone(), buffered_lines(state, &id.0)))
        .collect();
    TuiOutcome {
        last_run_code: state.exit_outcome(),
        last_summary: state.last_summary.clone(),
        failed_logs,
    }
}

fn buffered_lines(state: &AppState, beam: &str) -> Vec<String> {
    state
        .logs
        .get(beam)
        .map(|buffer| buffer.lines().map(|line| line.text.clone()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_engine::{BeamStatus, RunSummary};
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    fn id(name: &str) -> BeamId {
        BeamId(name.to_string())
    }

    fn run_started(beams: &[&str]) -> RunEvent {
        RunEvent::RunStarted {
            target: id("build"),
            beams: beams.iter().map(|name| id(name)).collect(),
            edges: Vec::new(),
        }
    }

    fn output(beam: &str, text: &str) -> RunEvent {
        RunEvent::BeamOutput {
            id: id(beam),
            line: alba_executors::OutputLine {
                stream: alba_executors::Stream::Stdout,
                text: text.to_string(),
            },
            replayed: false,
        }
    }

    fn finished(beam: &str, status: BeamStatus) -> RunEvent {
        RunEvent::BeamFinished {
            id: id(beam),
            status,
            duration: Duration::from_secs(1),
        }
    }

    fn summary_event(failed: &[&str], succeeded: &[&str]) -> RunEvent {
        RunEvent::RunFinished {
            summary: RunSummary {
                failed: failed.iter().map(|name| id(name)).collect(),
                succeeded: succeeded.iter().map(|name| id(name)).collect(),
                ..RunSummary::default()
            },
        }
    }

    /// A state with a run in flight over `beams`.
    fn running(beams: &[&str]) -> AppState {
        let mut state = AppState::new("build", false);
        state.apply(&run_started(beams), Instant::now());
        state
    }

    fn commands() -> (
        UnboundedSender<SessionCommand>,
        UnboundedReceiver<SessionCommand>,
    ) {
        unbounded_channel()
    }

    fn sent(receiver: &mut UnboundedReceiver<SessionCommand>) -> Vec<SessionCommand> {
        let mut commands = Vec::new();
        while let Ok(command) = receiver.try_recv() {
            commands.push(command);
        }
        commands
    }

    /// The terminal size these tests dispatch against — comfortably past
    /// the too-small floor, and the same 80x24 the render snapshots use.
    fn size() -> Size {
        Size::new(80, 24)
    }

    /// A run in flight forces the tick to keep redrawing on its own,
    /// same as before this task; an idle session with nothing to show
    /// does not.
    #[test]
    fn tick_should_redraw_while_a_run_is_in_flight() {
        let state = running(&["build"]);
        assert!(tick_should_redraw(&state));
    }

    #[test]
    fn tick_should_not_redraw_an_idle_session_with_no_copy_result() {
        let state = AppState::new("build", false);
        assert!(!tick_should_redraw(&state));
    }

    /// The regression this fixes: copy mode's most common use (reviewing
    /// a finished run, or the diagnostic while parked) has `running() ==
    /// false`, so without this the confirmation would sit on screen
    /// forever in an idle session — nothing would ever schedule the
    /// redraw that lets its two-second window expire.
    #[test]
    fn tick_should_redraw_an_idle_session_with_a_pending_copy_result() {
        let mut state = AppState::new("build", false);
        assert!(!state.running(), "precondition: idle session");
        state.record_copy_result("copied (OSC 52)", Instant::now());

        assert!(tick_should_redraw(&state));
    }

    #[test]
    fn clear_expired_copy_result_leaves_a_still_visible_one_alone() {
        let now = Instant::now();
        let mut state = AppState::new("build", false);
        state.record_copy_result("copied (OSC 52)", now);

        clear_expired_copy_result(&mut state, now + Duration::from_secs(1));

        assert!(
            state.last_copy_result.is_some(),
            "still within the two-second window"
        );
    }

    #[test]
    fn clear_expired_copy_result_drops_one_past_its_window() {
        let now = Instant::now();
        let mut state = AppState::new("build", false);
        state.record_copy_result("copied (OSC 52)", now);

        clear_expired_copy_result(&mut state, now + Duration::from_secs(3));

        assert!(
            state.last_copy_result.is_none(),
            "past the two-second window"
        );
    }

    /// The exit replay's raw material: each failed beam of the last
    /// summary, with what it printed.
    #[test]
    fn the_outcome_carries_failed_beams_buffers() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started(&["bad", "good"]), now);
        state.apply(&RunEvent::BeamStarted { id: id("bad") }, now);
        state.apply(&output("bad", "boom 1"), now);
        state.apply(&output("bad", "boom 2"), now);
        state.apply(&finished("bad", BeamStatus::Failed { exit_code: 1 }), now);
        state.apply(&RunEvent::BeamStarted { id: id("good") }, now);
        state.apply(&output("good", "fine"), now);
        state.apply(&finished("good", BeamStatus::Succeeded), now);
        state.apply(&summary_event(&["bad"], &["good"]), now);

        let outcome = outcome(&state);

        assert_eq!(outcome.last_run_code, Some(1));
        assert_eq!(
            outcome.failed_logs,
            vec![(
                "bad".to_string(),
                vec!["boom 1".to_string(), "boom 2".to_string()]
            )],
            "only the failed beam's lines, and all of them"
        );
        assert_eq!(
            outcome.last_summary.expect("a run finished").failed,
            vec![id("bad")]
        );
    }

    /// A session that quits before any run finished has nothing to
    /// replay and no code to offer — the CLI falls back to 130.
    #[test]
    fn a_session_with_no_finished_run_carries_no_outcome() {
        let state = running(&["build"]);

        let outcome = outcome(&state);

        assert_eq!(outcome.last_run_code, None);
        assert!(outcome.last_summary.is_none());
        assert!(outcome.failed_logs.is_empty());
    }

    /// A beam can fail without printing a line (a missing binary, a
    /// silent non-zero exit): it still belongs in the replay, with an
    /// empty body rather than a missing entry.
    #[test]
    fn a_failed_beam_that_printed_nothing_still_gets_an_entry() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started(&["bad"]), now);
        state.apply(&finished("bad", BeamStatus::Failed { exit_code: 127 }), now);
        state.apply(&summary_event(&["bad"], &[]), now);

        let outcome = outcome(&state);

        assert_eq!(outcome.failed_logs, vec![("bad".to_string(), Vec::new())]);
    }

    /// A mode with no renderer must not be enterable: the screen would
    /// not change, but the keymap would go modal behind a bottom bar
    /// still advertising the Normal-mode keys. Copy is no longer among
    /// them — this task gives it a renderer, so `EnterCopy` now has its
    /// own test below instead.
    #[test]
    fn modes_with_no_renderer_are_not_enterable_yet() {
        let (commands, _receiver) = commands();
        let mut state = AppState::new("build", false);

        for action in [Action::EnterGraph, Action::EnterHelp] {
            dispatch(&mut state, &commands, action, size());
            assert_eq!(state.mode, state::Mode::Normal, "{action:?} entered a mode");
        }
    }

    /// `v`: now that copy mode has a renderer, entering it actually
    /// flips the mode, anchored at the log pane's own top visible line.
    #[test]
    fn entering_copy_mode_anchors_at_the_panes_top_line() {
        let (commands, _receiver) = commands();
        let mut state = running(&["build"]);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, Instant::now());
        for index in 0..30 {
            state.apply(&output("build", &format!("line {index}")), Instant::now());
        }

        dispatch(&mut state, &commands, Action::EnterCopy, size());

        match &state.mode {
            state::Mode::Copy(copy) => {
                // 80x24 gives the log pane a content height of 20 rows
                // (see `ui::log_pane_content_area`); following a 30-line
                // buffer, the top visible line is 30 - 20 = 10.
                assert_eq!(copy.anchor, (10, 0));
                assert_eq!(copy.cursor, (10, 0));
            }
            other => panic!("expected Mode::Copy, got {other:?}"),
        }
    }

    /// A drag inside the log pane's content area anchors on `Down` and
    /// extends the cursor on `Drag`, hit-tested through the same layout
    /// `ui::log_pane_content_area` computes. `Up` (release) is not
    /// exercised here: it copies to the clipboard, which is the pseudo
    /// terminal smoke test's job, not a unit test's.
    #[test]
    fn a_mouse_drag_in_copy_mode_selects_by_pane_row() {
        let (commands, _receiver) = commands();
        let mut state = running(&["build"]);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, Instant::now());
        for index in 0..5 {
            state.apply(&output("build", &format!("line {index}")), Instant::now());
        }
        dispatch(&mut state, &commands, Action::EnterCopy, size());

        // The log pane's content area starts at (32, 2) for an 80x24
        // terminal (see `ui::log_pane_content_area`); its 5 lines all
        // fit inside the 20-row content height and follow the tail, so
        // row 0 of the pane is buffer line 0 ("line 0").
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 34,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        dispatch(&mut state, &commands, Action::Mouse(down), size());
        match &state.mode {
            state::Mode::Copy(copy) => assert_eq!(copy.anchor, (0, 2)),
            other => panic!("expected Mode::Copy, got {other:?}"),
        }

        let drag = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            column: 34,
            row: 4,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        dispatch(&mut state, &commands, Action::Mouse(drag), size());
        match &state.mode {
            state::Mode::Copy(copy) => {
                assert_eq!(copy.anchor, (0, 2), "the anchor stays put");
                assert_eq!(copy.cursor, (2, 2), "the cursor follows the drag");
            }
            other => panic!("expected Mode::Copy, got {other:?}"),
        }
    }

    /// A click outside the log pane's content area (here, in the tree
    /// pane's own columns) hits nothing: copy mode's selection only ever
    /// addresses the log buffer beside it.
    #[test]
    fn a_click_outside_the_log_pane_does_nothing() {
        let (commands, _receiver) = commands();
        let mut state = running(&["build"]);
        dispatch(&mut state, &commands, Action::EnterCopy, size());
        let before = match &state.mode {
            state::Mode::Copy(copy) => *copy,
            other => panic!("expected Mode::Copy, got {other:?}"),
        };

        let click_in_tree = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        dispatch(&mut state, &commands, Action::Mouse(click_in_tree), size());

        match &state.mode {
            state::Mode::Copy(copy) => assert_eq!(*copy, before, "the click changed nothing"),
            other => panic!("expected Mode::Copy, got {other:?}"),
        }
    }

    /// A release ending outside the log pane — dragging past the last
    /// line or into the tree pane's own columns, and letting go there —
    /// still finishes the copy. `dispatch` itself never touches the
    /// clipboard (that is `lib::run`'s job, after `dispatch` returns),
    /// so this only has to prove `pending_copy` ends up holding the
    /// selection rather than being left stranded.
    #[test]
    fn a_release_outside_the_log_pane_still_stages_the_copy() {
        let (commands, _receiver) = commands();
        let mut state = running(&["build"]);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, Instant::now());
        state.apply(&output("build", "alpha"), Instant::now());
        dispatch(&mut state, &commands, Action::EnterCopy, size());

        // Drag once inside the pane so the selection covers something.
        let down = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 34,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        dispatch(&mut state, &commands, Action::Mouse(down), size());

        // Release far outside the pane, in the tree pane's own columns.
        let up = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        dispatch(&mut state, &commands, Action::Mouse(up), size());

        assert_eq!(state.mode, state::Mode::Normal);
        assert_eq!(state.pending_copy.as_deref(), Some("p"));
    }

    /// `q` on a run in flight abandons it: the summary that lands after
    /// the loop's drain must not vouch for sources it never finished
    /// checking.
    #[test]
    fn quitting_mid_run_abandons_the_run() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["build"]);

        dispatch(&mut state, &commands, Action::Quit, size());

        assert!(state.should_quit);
        state.apply(&summary_event(&[], &["build"]), Instant::now());
        assert_eq!(
            state.exit_outcome(),
            None,
            "an abandoned run scores 0 all the same; it just may not vouch"
        );
        assert!(
            sent(&mut receiver).is_empty(),
            "the loop's own Shutdown ends the run; q sends nothing itself"
        );
    }

    /// `q` with nothing running concludes the session: the last run's
    /// code stands.
    #[test]
    fn quitting_while_idle_keeps_the_last_runs_code() {
        let (commands, _receiver) = commands();
        let mut state = running(&["build"]);
        state.apply(&summary_event(&["build"], &[]), Instant::now());

        dispatch(&mut state, &commands, Action::Quit, size());

        assert!(state.should_quit);
        assert_eq!(state.exit_outcome(), Some(1));
    }

    /// Ctrl-C means cancel while a run is in flight — the session stays
    /// alive, waiting.
    #[test]
    fn ctrl_c_cancels_the_run_in_flight() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["build"]);

        dispatch(&mut state, &commands, Action::CancelOrQuit, size());

        assert!(!state.should_quit, "cancelling is not quitting");
        assert!(matches!(
            sent(&mut receiver)[..],
            [SessionCommand::CancelRun]
        ));
        state.apply(&summary_event(&[], &[]), Instant::now());
        assert_eq!(state.exit_outcome(), None, "the cancelled run cannot vouch");
    }

    /// Ctrl-C with nothing running quits, and interrupts rather than
    /// concludes: even a green run behind it no longer vouches.
    #[test]
    fn ctrl_c_quits_via_interrupt_when_nothing_runs() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["build"]);
        state.apply(&summary_event(&[], &["build"]), Instant::now());
        assert_eq!(state.exit_outcome(), Some(0), "the run went green first");

        dispatch(&mut state, &commands, Action::CancelOrQuit, size());

        assert!(state.should_quit);
        assert_eq!(state.exit_outcome(), None);
        assert!(sent(&mut receiver).is_empty(), "there is nothing to cancel");
    }

    /// `c` on an idle session is a no-op: there is no run to cancel, and
    /// the last one's verdict still stands.
    #[test]
    fn cancelling_while_idle_changes_nothing() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["build"]);
        state.apply(&summary_event(&[], &["build"]), Instant::now());

        dispatch(&mut state, &commands, Action::CancelRun, size());

        assert!(!state.should_quit);
        assert_eq!(state.exit_outcome(), Some(0));
        assert!(sent(&mut receiver).is_empty());
    }

    /// `r` and `f` re-run the *selected* beam, and `f` is the one that
    /// forces.
    #[test]
    fn rerunning_asks_the_session_for_the_selected_beam() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["codegen", "build"]);
        state.select(1);
        state.apply(&summary_event(&[], &["build"]), Instant::now());

        dispatch(
            &mut state,
            &commands,
            Action::Rerun { force: false },
            size(),
        );
        dispatch(&mut state, &commands, Action::Rerun { force: true }, size());

        match &sent(&mut receiver)[..] {
            [
                SessionCommand::RunBeam {
                    id: first,
                    force: false,
                },
                SessionCommand::RunBeam {
                    id: second,
                    force: true,
                },
            ] => {
                assert_eq!(first, &id("build"));
                assert_eq!(second, &id("build"));
            }
            other => panic!("expected two RunBeam commands, got {other:?}"),
        }
    }

    /// A rerun supersedes the run in flight, which therefore may not
    /// vouch either — the session cancels it, the state disowns it.
    #[test]
    fn rerunning_over_a_run_in_flight_abandons_it() {
        let (commands, _receiver) = commands();
        let mut state = running(&["build"]);

        dispatch(
            &mut state,
            &commands,
            Action::Rerun { force: false },
            size(),
        );

        state.apply(&summary_event(&[], &[]), Instant::now());
        assert_eq!(state.exit_outcome(), None);
    }

    /// Nothing selected (before the first run) means nothing to re-run.
    #[test]
    fn rerunning_with_no_selection_sends_nothing() {
        let (commands, mut receiver) = commands();
        let mut state = AppState::new("build", false);

        dispatch(
            &mut state,
            &commands,
            Action::Rerun { force: false },
            size(),
        );

        assert!(sent(&mut receiver).is_empty());
    }

    /// `w` flips the header *and* tells the session: a watch toggle the
    /// engine never hears about would be a lie on screen.
    #[test]
    fn toggling_watch_flips_the_state_and_tells_the_session() {
        let (commands, mut receiver) = commands();
        let mut state = AppState::new("build", false);

        dispatch(&mut state, &commands, Action::ToggleWatch, size());
        assert!(state.watch_enabled);
        dispatch(&mut state, &commands, Action::ToggleWatch, size());
        assert!(!state.watch_enabled);

        assert!(matches!(
            sent(&mut receiver)[..],
            [
                SessionCommand::SetWatch(true),
                SessionCommand::SetWatch(false)
            ]
        ));
    }

    /// Scrolling addresses the selected beam's buffer, so moving the
    /// selection moves what `G` and the wheel act on.
    #[test]
    fn scrolling_acts_on_the_selected_beams_buffer() {
        let (commands, _receiver) = commands();
        let mut state = running(&["codegen", "build"]);
        let now = Instant::now();
        for index in 0..5 {
            state.apply(&output("codegen", &format!("line {index}")), now);
        }

        dispatch(&mut state, &commands, Action::ScrollUp(2), size());
        assert!(matches!(
            state.logs["codegen"].scroll(),
            logs::Scroll::Paused { offset: 2 }
        ));

        dispatch(&mut state, &commands, Action::FollowTail, size());
        assert!(matches!(
            state.logs["codegen"].scroll(),
            logs::Scroll::Following
        ));
    }
}
