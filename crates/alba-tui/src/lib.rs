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
use futures::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::input::Action;
use crate::logs::LogBuffer;
use crate::state::AppState;

/// The heartbeat between events. Redrawing is event-driven, but elapsed
/// times and the progress bar have to keep moving while nothing arrives,
/// so the loop wakes on its own this often and redraws only while a run
/// is in flight — an idle screen shows nothing that a clock changes.
const TICK: Duration = Duration::from_millis(80);

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
        }

        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    let now = Instant::now();
                    state.apply(&event, now);
                    // Whatever is already queued behind it folds into the
                    // same frame: a beam flooding its output must cost one
                    // redraw, not one per line.
                    while let Ok(event) = events.try_recv() {
                        state.apply(&event, now);
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
                    dispatch(&mut state, &commands, action);
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
                // Only a run in flight has something a clock changes.
                dirty = state.running();
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

/// The one place a user intent meets the state and the session.
///
/// Kept apart from the loop — and free of any terminal of its own — so
/// the rules the exit code depends on can be tested by handing it an
/// action and reading back what it did.
fn dispatch(state: &mut AppState, commands: &UnboundedSender<SessionCommand>, action: Action) {
    match action {
        Action::Quit => {
            // Quitting on a run in flight abandons it: its summary,
            // whatever it ends up saying, is not a verdict on the sources.
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
                // has to know the superseded run may not vouch.
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
        // Copy and Graph have neither state nor a renderer yet. Entering
        // a mode nothing draws would leave the user facing an unchanged
        // screen whose keys no longer do what the bottom bar says, so
        // until those modes exist their keys do nothing at all.
        Action::EnterCopy | Action::EnterGraph => {}
        Action::EnterHelp => state.mode = state::Mode::Help,
        Action::LeaveMode => state.leave_mode(),
        Action::Key(key) => state.handle_modal_key(key),
        // Click-to-select and drag-to-copy arrive with the copy mode that
        // owns the hit testing they need.
        Action::Mouse(_) => {}
        Action::None => {}
    }
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

    /// `q` on a run in flight abandons it: the summary that lands after
    /// the loop's drain must not vouch for sources it never finished
    /// checking.
    #[test]
    fn quitting_mid_run_abandons_the_run() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["build"]);

        dispatch(&mut state, &commands, Action::Quit);

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

        dispatch(&mut state, &commands, Action::Quit);

        assert!(state.should_quit);
        assert_eq!(state.exit_outcome(), Some(1));
    }

    /// Ctrl-C means cancel while a run is in flight — the session stays
    /// alive, waiting.
    #[test]
    fn ctrl_c_cancels_the_run_in_flight() {
        let (commands, mut receiver) = commands();
        let mut state = running(&["build"]);

        dispatch(&mut state, &commands, Action::CancelOrQuit);

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

        dispatch(&mut state, &commands, Action::CancelOrQuit);

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

        dispatch(&mut state, &commands, Action::CancelRun);

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

        dispatch(&mut state, &commands, Action::Rerun { force: false });
        dispatch(&mut state, &commands, Action::Rerun { force: true });

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

        dispatch(&mut state, &commands, Action::Rerun { force: false });

        state.apply(&summary_event(&[], &[]), Instant::now());
        assert_eq!(state.exit_outcome(), None);
    }

    /// Nothing selected (before the first run) means nothing to re-run.
    #[test]
    fn rerunning_with_no_selection_sends_nothing() {
        let (commands, mut receiver) = commands();
        let mut state = AppState::new("build", false);

        dispatch(&mut state, &commands, Action::Rerun { force: false });

        assert!(sent(&mut receiver).is_empty());
    }

    /// `w` flips the header *and* tells the session: a watch toggle the
    /// engine never hears about would be a lie on screen.
    #[test]
    fn toggling_watch_flips_the_state_and_tells_the_session() {
        let (commands, mut receiver) = commands();
        let mut state = AppState::new("build", false);

        dispatch(&mut state, &commands, Action::ToggleWatch);
        assert!(state.watch_enabled);
        dispatch(&mut state, &commands, Action::ToggleWatch);
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

        dispatch(&mut state, &commands, Action::ScrollUp(2));
        assert!(matches!(
            state.logs["codegen"].scroll(),
            logs::Scroll::Paused { offset: 2 }
        ));

        dispatch(&mut state, &commands, Action::FollowTail);
        assert!(matches!(
            state.logs["codegen"].scroll(),
            logs::Scroll::Following
        ));
    }
}
