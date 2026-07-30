//! Everything the screen shows, folded out of the engine's event stream.
//!
//! This module is pure: it never touches the terminal, never reads a
//! clock, and never runs anything. `now` is handed to [`AppState::apply`]
//! rather than sampled, so the whole semantic core of the TUI — statuses,
//! progress, logs, and the exit code the process will end on — is a
//! function of its inputs, and tests own the clock.
//!
//! The subtle part is the *outcome machine*. `q` exits with the last
//! run's code only when that run ran to completion un-abandoned; every
//! other case falls back to the interrupted code the CLI applies. An
//! older green run must not vouch for sources that have changed since,
//! so a run in flight, a user cancellation, a watch trigger that
//! superseded a run, a parked project, and Ctrl-C while idle all
//! withdraw the outcome. The mapping from a finished run to its code
//! stays where it already lives, in [`RunSummary::exit_code`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use alba_core::BeamId;
use alba_engine::{BeamStatus, RunEvent, RunSummary};

use crate::logs::LogBuffer;

/// The log buffer the parked diagnostic goes to: a pseudo-beam, so a
/// diagnostic that belongs to no beam still has a pane to be read in.
pub const DIAGNOSTIC_LOG: &str = "alba";

#[derive(Debug, Clone)]
pub enum BeamState {
    Pending,
    Running {
        since: Instant,
    },
    Done {
        status: BeamStatus,
        duration: Duration,
    },
}

#[derive(Debug, Clone)]
pub struct BeamRow {
    pub id: String,
    pub state: BeamState,
}

/// What the header shows.
#[derive(Debug, Clone)]
pub enum Phase {
    /// A run is in flight. `done`/`total` feed the progress bar.
    Running {
        done: usize,
        total: usize,
        since: Instant,
    },
    /// The last run ended; its summary is on screen. Non-watch idle state.
    Finished,
    /// Watch is on and the session waits on `files` watched files.
    Waiting { files: usize },
    /// The project is broken; the diagnostic is in the log pane.
    Parked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Search,
    Copy,
    Graph,
    Help,
}

pub struct AppState {
    pub target: String,
    pub beams: Vec<BeamRow>,
    /// (beam index, dependency index) into `beams`.
    pub edges: Vec<(usize, usize)>,
    pub selected: usize,
    pub logs: HashMap<String, LogBuffer>,
    pub phase: Phase,
    pub watch_enabled: bool,
    pub mode: Mode,
    pub last_summary: Option<RunSummary>,
    pub should_quit: bool,
    /// The code the last run earned, or `None` when no run may vouch.
    outcome: Option<i32>,
    /// The run in flight has been abandoned by the user; its summary,
    /// whatever it says, is not a verdict on the sources.
    user_cancelled: bool,
    /// A `WatchWaiting` was seen since the run started, which is how a
    /// watch trigger that follows a completed run is told apart from one
    /// that superseded a run still going.
    waiting_seen: bool,
}

impl AppState {
    pub fn new(target: &str, watch_enabled: bool) -> Self {
        Self {
            target: target.to_string(),
            beams: Vec::new(),
            edges: Vec::new(),
            selected: 0,
            logs: HashMap::new(),
            // Nothing has run yet, so the session is idle rather than in
            // flight — and `outcome` is `None`, which is what makes `q`
            // fall back to the interrupted code: no run vouches for a
            // session that never ran one.
            phase: Phase::Finished,
            watch_enabled,
            mode: Mode::Normal,
            last_summary: None,
            should_quit: false,
            outcome: None,
            user_cancelled: false,
            waiting_seen: false,
        }
    }

    /// Folds one event in. `now` is passed, not sampled — the state stays
    /// a pure function of its inputs, and tests pick the clock.
    pub fn apply(&mut self, event: &RunEvent, now: Instant) {
        match event {
            RunEvent::RunStarted {
                target,
                beams,
                edges,
            } => self.start_run(target.0.clone(), beams, edges, now),
            RunEvent::BeamStarted { id } | RunEvent::BeamCached { id } => {
                if let Some(row) = self.row_mut(&id.0) {
                    row.state = BeamState::Running { since: now };
                }
                // A rerun starts the beam's story fresh: whatever the
                // previous attempt printed is no longer what happened.
                self.logs.entry(id.0.clone()).or_default().clear();
            }
            RunEvent::BeamOutput { id, line, replayed } => {
                self.logs
                    .entry(id.0.clone())
                    .or_default()
                    .push(line.text.clone(), *replayed);
            }
            RunEvent::BeamFinished {
                id,
                status,
                duration,
            } => {
                let Some(row) = self.row_mut(&id.0) else {
                    return;
                };
                row.state = BeamState::Done {
                    status: status.clone(),
                    duration: *duration,
                };
                if let Phase::Running { done, total, .. } = &mut self.phase {
                    *done = (*done + 1).min(*total);
                }
            }
            RunEvent::RunFinished { summary } => {
                self.phase = Phase::Finished;
                self.last_summary = Some(summary.clone());
                // An abandoned run scores 0 all the same (cancelled beams
                // do not fail), which is exactly why the flag, not the
                // summary, decides whether it may vouch.
                self.outcome = (!self.user_cancelled).then(|| summary.exit_code());
            }
            RunEvent::WatchWaiting { files } => {
                // A parked session watches too, and resolves no file: its
                // header must keep saying why nothing runs.
                if !matches!(self.phase, Phase::Parked) || *files > 0 {
                    self.phase = Phase::Waiting { files: *files };
                }
                self.waiting_seen = true;
            }
            RunEvent::WatchTriggered { .. } => {
                // No wait since the last run ended means the watch cut
                // that run short: its summary was a list of cancellations.
                if !self.waiting_seen {
                    self.outcome = None;
                }
                self.waiting_seen = false;
            }
            RunEvent::ProjectBroken { diagnostic } => {
                self.phase = Phase::Parked;
                self.outcome = None;
                let buffer = self.logs.entry(DIAGNOSTIC_LOG.to_string()).or_default();
                for line in diagnostic.lines() {
                    buffer.push(line.to_string(), false);
                }
            }
        }
    }

    /// The TUI is about to abandon the run in flight (CancelRun, Ctrl-C,
    /// or a RunBeam that supersedes it): its summary must not vouch.
    pub fn mark_user_cancelled(&mut self) {
        // Only a run in flight can be abandoned; outside one there is
        // nothing to withdraw, and the last run's verdict still stands.
        if self.running() {
            self.user_cancelled = true;
        }
    }

    /// The exit code `q` earns right now: `Some(code)` iff the last run
    /// ran to completion un-abandoned.
    pub fn exit_outcome(&self) -> Option<i32> {
        self.outcome
    }

    /// Ctrl-C with nothing running: the session is being interrupted,
    /// not concluded — `exit_outcome()` returns `None` from here on
    /// (spec: Ctrl-C when idle quits with 130, even after a green run).
    pub fn quit_via_interrupt(&mut self) {
        self.outcome = None;
        // Should an event still be in flight behind the interrupt, its
        // run counts as abandoned too: the interrupt ends the session,
        // it does not conclude it.
        self.user_cancelled = true;
        self.should_quit = true;
    }

    pub fn selected_beam(&self) -> Option<&BeamRow> {
        self.beams.get(self.selected)
    }

    pub fn select_next(&mut self) {
        self.select(self.selected + 1);
    }

    pub fn select_previous(&mut self) {
        self.select(self.selected.saturating_sub(1));
    }

    /// Moves the selection, clamped to the table. The ends of the tree
    /// stop rather than wrap: a list the reader scans top to bottom is
    /// easier to keep a place in when it does not loop under them.
    pub fn select(&mut self, index: usize) {
        self.selected = index.min(self.beams.len().saturating_sub(1));
    }

    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    /// The table is rebuilt from every `RunStarted` rather than patched:
    /// a watch session reloads the Beamfile, so the beams and the edges
    /// of the next run are not necessarily those of the last.
    fn start_run(
        &mut self,
        target: String,
        beams: &[BeamId],
        edges: &[(BeamId, BeamId)],
        now: Instant,
    ) {
        self.target = target;
        self.beams = beams
            .iter()
            .map(|id| BeamRow {
                id: id.0.clone(),
                state: BeamState::Pending,
            })
            .collect();
        self.edges = edges
            .iter()
            .filter_map(|(beam, dependency)| {
                Some((self.index_of(&beam.0)?, self.index_of(&dependency.0)?))
            })
            .collect();
        self.select(self.selected);
        self.phase = Phase::Running {
            done: 0,
            total: self.beams.len(),
            since: now,
        };
        self.outcome = None;
        self.user_cancelled = false;
        self.waiting_seen = false;
        // A run starting is the project loading again: the diagnostic
        // that parked the session describes a Beamfile that no longer is.
        if let Some(buffer) = self.logs.get_mut(DIAGNOSTIC_LOG) {
            buffer.clear();
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.beams.iter().position(|row| row.id == id)
    }

    fn row_mut(&mut self, id: &str) -> Option<&mut BeamRow> {
        self.beams.iter_mut().find(|row| row.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::{BeamStatus, RunEvent, RunSummary};
    use std::time::{Duration, Instant};

    fn id(name: &str) -> BeamId {
        BeamId(name.to_string())
    }

    fn run_started(target: &str, beams: &[&str], edges: &[(&str, &str)]) -> RunEvent {
        RunEvent::RunStarted {
            target: id(target),
            beams: beams.iter().map(|name| id(name)).collect(),
            edges: edges.iter().map(|(a, b)| (id(a), id(b))).collect(),
        }
    }

    fn finished(status: BeamStatus, beam: &str) -> RunEvent {
        RunEvent::BeamFinished {
            id: id(beam),
            status,
            duration: Duration::from_secs(1),
        }
    }

    fn summary_event(failed: &[&str]) -> RunEvent {
        RunEvent::RunFinished {
            summary: RunSummary {
                failed: failed.iter().map(|name| id(name)).collect(),
                ..RunSummary::default()
            },
        }
    }

    /// RunStarted rebuilds the table: the Beamfile may have changed.
    #[test]
    fn run_started_builds_the_beam_table_and_progress() {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["codegen", "build"], &[("build", "codegen")]),
            Instant::now(),
        );
        assert_eq!(state.beams.len(), 2);
        assert_eq!(state.edges, vec![(1, 0)]);
        assert!(matches!(
            state.phase,
            Phase::Running {
                done: 0,
                total: 2,
                ..
            }
        ));
    }

    /// Statuses walk their lifecycle; done counts drive the bar.
    #[test]
    fn beam_events_advance_statuses_and_the_bar() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["codegen", "build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
        assert!(matches!(state.beams[0].state, BeamState::Running { .. }));
        state.apply(&finished(BeamStatus::Succeeded, "codegen"), now);
        assert!(matches!(
            state.phase,
            Phase::Running {
                done: 1,
                total: 2,
                ..
            }
        ));
        // a cache hit is a finish too
        state.apply(&RunEvent::BeamCached { id: id("build") }, now);
        state.apply(&finished(BeamStatus::Cached, "build"), now);
        assert!(matches!(
            state.phase,
            Phase::Running {
                done: 2,
                total: 2,
                ..
            }
        ));
    }

    /// Output lands in the right buffer; a rerun clears it first.
    #[test]
    fn a_rerun_starts_the_beams_buffer_fresh() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "old line"), now);
        state.apply(&finished(BeamStatus::Succeeded, "build"), now);
        state.apply(&summary_event(&[]), now);
        // second run
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        assert_eq!(state.logs.get("build").unwrap().len(), 0);
    }

    /// The spec's exit rule, case by case.
    #[test]
    fn a_completed_run_vouches_for_q() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&[]), now);
        assert_eq!(state.exit_outcome(), Some(0));
    }

    #[test]
    fn a_failed_run_vouches_with_its_own_code() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&["build"]), now);
        assert_eq!(state.exit_outcome(), Some(1));
    }

    /// Quitting mid-run: the previous green run must not vouch.
    #[test]
    fn a_run_in_flight_withdraws_the_previous_outcome() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&[]), now);
        state.apply(&run_started("build", &["build"], &[]), now);
        assert_eq!(state.exit_outcome(), None);
    }

    /// A user-cancelled run never vouches, even though its summary's own
    /// code is 0 (cancelled beams score 0).
    #[test]
    fn a_user_cancelled_run_does_not_vouch() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.mark_user_cancelled();
        state.apply(&summary_event(&[]), now);
        assert_eq!(state.exit_outcome(), None);
    }

    /// A watch trigger that arrives with no WatchWaiting since the last
    /// RunFinished is the engine saying "that run was interrupted".
    #[test]
    fn a_supersede_trigger_withdraws_the_outcome() {
        let mut state = AppState::new("build", true);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&[]), now);
        state.apply(
            &RunEvent::WatchTriggered {
                paths: vec!["src/a.rs".into()],
            },
            now,
        );
        assert_eq!(state.exit_outcome(), None);
    }

    /// The ordinary watch cycle keeps the outcome: waited, then triggered.
    #[test]
    fn a_trigger_after_waiting_does_not_withdraw() {
        let mut state = AppState::new("build", true);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&[]), now);
        state.apply(&RunEvent::WatchWaiting { files: 3 }, now);
        assert!(matches!(state.phase, Phase::Waiting { files: 3 }));
        state.apply(&RunEvent::WatchTriggered { paths: vec![] }, now);
        assert_eq!(state.exit_outcome(), Some(0), "that run did complete");
    }

    /// Ctrl-C when idle is an interruption, not a conclusion: even a
    /// completed green run stops vouching.
    #[test]
    fn ctrl_c_idle_quits_without_vouching() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&[]), now);
        assert_eq!(state.exit_outcome(), Some(0), "precondition");
        state.quit_via_interrupt();
        assert_eq!(state.exit_outcome(), None);
    }

    /// ProjectBroken parks the header and shows the diagnostic.
    #[test]
    fn project_broken_parks_and_logs_the_diagnostic() {
        let mut state = AppState::new("build", true);
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\n".to_string(),
            },
            Instant::now(),
        );
        assert!(matches!(state.phase, Phase::Parked));
        assert_eq!(
            state.exit_outcome(),
            None,
            "a parked session does not vouch"
        );
    }

    /// A cancellation outside a run has nothing to abandon: the last run
    /// still vouches.
    #[test]
    fn a_cancellation_outside_a_run_does_not_withdraw() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&summary_event(&[]), now);
        state.mark_user_cancelled();
        assert_eq!(state.exit_outcome(), Some(0));
    }

    /// The reader's place in the tree survives a reload that shortens it.
    #[test]
    fn a_shorter_table_clamps_the_selection() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["codegen", "build"], &[]), now);
        state.select(1);
        state.apply(&run_started("build", &["build"], &[]), now);
        assert_eq!(state.selected, 0);
        assert_eq!(
            state.selected_beam().map(|row| row.id.as_str()),
            Some("build")
        );
    }

    /// The ends of the tree stop rather than wrap.
    #[test]
    fn selection_stops_at_both_ends() {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["codegen", "build"], &[]),
            Instant::now(),
        );
        state.select_previous();
        assert_eq!(state.selected, 0);
        state.select_next();
        state.select_next();
        assert_eq!(state.selected, 1);
    }

    /// A parked session still watches, and resolves no file: the header
    /// must keep saying why nothing runs.
    #[test]
    fn a_parked_session_stays_parked_while_it_waits() {
        let mut state = AppState::new("build", true);
        let now = Instant::now();
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\n".to_string(),
            },
            now,
        );
        state.apply(&RunEvent::WatchWaiting { files: 0 }, now);
        assert!(matches!(state.phase, Phase::Parked));
    }

    /// A run starting is the project loading again: the diagnostic that
    /// parked the session describes a Beamfile that no longer is.
    #[test]
    fn a_reload_clears_the_parked_diagnostic() {
        let mut state = AppState::new("build", true);
        let now = Instant::now();
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\nhelp: fix it\n".to_string(),
            },
            now,
        );
        assert_eq!(state.logs.get(DIAGNOSTIC_LOG).unwrap().len(), 2);
        state.apply(&run_started("build", &["build"], &[]), now);
        assert_eq!(state.logs.get(DIAGNOSTIC_LOG).unwrap().len(), 0);
        assert!(state.running());
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
}
