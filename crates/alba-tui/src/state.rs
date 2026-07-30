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
use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};

use crate::copy::CopyState;
use crate::logs::LogBuffer;
use crate::search::SearchState;

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
    Search(SearchState),
    Copy(CopyState),
    Graph,
    Help,
}

/// A committed search, tied to the beam it ran against. `search.matches`
/// are line indices into whatever that beam's buffer held at the time —
/// meaningless against a different beam's content, or against the same
/// beam's buffer after a rerun has cleared it. `n`/`N` compare `beam`
/// against the current selection before trusting `matches`, rather than
/// stepping stale indices into whatever happens to be on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommittedSearch {
    pub beam: String,
    pub search: SearchState,
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
    /// The most recently active search. `Mode::Search` carries the one
    /// being typed; this is where it lands once `Esc` or `Enter` leaves
    /// that mode, so the log pane keeps highlighting its matches until
    /// the next search session (`enter_search`) starts a fresh, empty
    /// one and replaces it.
    pub last_search: Option<CommittedSearch>,
    /// What `y` (or a mouse release) last put on the clipboard, shown in
    /// the bottom bar for exactly the one draw that follows — `run`'s
    /// loop clears it right after that draw, which is what keeps it from
    /// lingering past the frame it was meant for.
    pub last_copy_result: Option<&'static str>,
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
            last_search: None,
            last_copy_result: None,
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
                // A committed search's matches are line indices into
                // whatever that beam's buffer held when it was
                // committed — the rerun just erased that, so stepping
                // into them would silently pause the pane at an offset
                // that describes nothing on screen. Recomputing against
                // the now-empty buffer collapses `matches` instead.
                if let Some(mut committed) = self.last_search.take() {
                    if committed.beam == id.0 {
                        self.recompute_search_against(&id.0, &mut committed.search);
                    }
                    self.last_search = Some(committed);
                }
            }
            RunEvent::BeamOutput { id, line, replayed } => {
                self.logs
                    .entry(id.0.clone())
                    .or_default()
                    .push(line.text.clone(), *replayed);
                // A still-running beam's search must not go stale while
                // the user is not typing: a keystroke is not the only
                // way the match set can change, the buffer gaining a
                // matching line is another.
                if self.selected_beam().is_some_and(|row| row.id == id.0) {
                    self.resync_active_search();
                }
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

    /// The selected beam's id, or an empty string when there is none —
    /// the same fallback `ui/logpane.rs` uses for "nothing to show".
    fn current_beam_id(&self) -> String {
        self.selected_beam()
            .map(|row| row.id.clone())
            .unwrap_or_default()
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

    /// `/`: opens a fresh search over the selected beam's buffer. Any
    /// previous session's matches stay in `last_search` until this one
    /// is itself left, which is what keeps the log pane highlighted
    /// right up to the moment a new search replaces it.
    pub fn enter_search(&mut self) {
        self.mode = Mode::Search(SearchState::new());
    }

    /// `v`: opens copy mode, anchored at whatever line the log pane's
    /// current scroll window shows at its own top — `pane_height` is the
    /// log pane's content height for the terminal size `dispatch` has in
    /// hand, the one piece of screen geometry `AppState` itself does not
    /// otherwise need to know.
    pub fn enter_copy(&mut self, pane_height: usize) {
        let beam = self.current_beam_id();
        let top_line = self
            .logs
            .get(&beam)
            .map(|buffer| crate::copy::top_visible_line(buffer, pane_height))
            .unwrap_or(0);
        self.mode = Mode::Copy(CopyState::new_at(top_line));
    }

    /// `Esc`: leaves whatever modal mode is active. Search stashes its
    /// state into `last_search` first, tagged with the beam it ran
    /// against; the other modal modes carry no payload, so there is
    /// nothing to keep.
    pub fn leave_mode(&mut self) {
        if let Mode::Search(search) = std::mem::replace(&mut self.mode, Mode::Normal) {
            self.last_search = Some(CommittedSearch {
                beam: self.current_beam_id(),
                search,
            });
        }
    }

    /// The modal keymap's entry point: routes a key event to whichever
    /// mode is active. Graph and Help belong to the tasks that give
    /// those modes behaviour.
    pub fn handle_modal_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Search(_) => self.handle_search_key(key),
            Mode::Copy(_) => self.handle_copy_key(key),
            _ => {}
        }
    }

    /// Search's own keymap while composing: every printable character —
    /// `n`/`N` included — edits the query, `Backspace` erases from it,
    /// and `Enter` commits the query and leaves search mode the same way
    /// `Esc` does (via `leave_mode`), keeping the highlights in
    /// `last_search`. Stepping is a Normal-mode binding on the committed
    /// search (`search_next`/`search_previous`), not something typed
    /// here — the vim/less split, so the query itself can still contain
    /// `n` or `N`.
    fn handle_search_key(&mut self, key: KeyEvent) {
        let Mode::Search(mut search) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("handle_modal_key only calls this while Mode::Search is active")
        };
        match key.code {
            KeyCode::Enter => {
                self.last_search = Some(CommittedSearch {
                    beam: self.current_beam_id(),
                    search,
                });
                return;
            }
            KeyCode::Char(character) => {
                search.push_char(character);
                self.recompute_search(&mut search);
            }
            KeyCode::Backspace => {
                search.pop_char();
                self.recompute_search(&mut search);
            }
            _ => {}
        }
        self.sync_search_scroll(&search);
        self.mode = Mode::Search(search);
    }

    /// Copy mode's own keymap: arrows and `hjkl` move the cursor, `v`
    /// re-anchors the selection at wherever the cursor currently sits
    /// (starting a fresh span from there), and `y` copies the selected
    /// text and returns to Normal — `Esc` leaving without copying is
    /// `leave_mode`'s job, same as every other modal mode.
    fn handle_copy_key(&mut self, key: KeyEvent) {
        let Mode::Copy(mut copy) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("handle_modal_key only calls this while Mode::Copy is active")
        };
        let beam = self.current_beam_id();
        match key.code {
            KeyCode::Char('v') => copy.anchor = copy.cursor,
            KeyCode::Char('y') => {
                if let Some(buffer) = self.logs.get(&beam) {
                    let text = copy.selected_text(buffer);
                    self.last_copy_result = Some(crate::copy::copy_to_clipboard(&text));
                }
                return; // stays Mode::Normal, already replaced above
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_copy_cursor(&mut copy, -1, 0),
            KeyCode::Down | KeyCode::Char('j') => self.move_copy_cursor(&mut copy, 1, 0),
            KeyCode::Left | KeyCode::Char('h') => self.move_copy_cursor(&mut copy, 0, -1),
            KeyCode::Right | KeyCode::Char('l') => self.move_copy_cursor(&mut copy, 0, 1),
            _ => {}
        }
        self.mode = Mode::Copy(copy);
    }

    fn move_copy_cursor(&self, copy: &mut CopyState, dl: isize, dc: isize) {
        let beam = self.current_beam_id();
        if let Some(buffer) = self.logs.get(&beam) {
            copy.move_cursor(dl, dc, buffer);
        }
    }

    /// Copy mode's mouse entry point: `pane_row`/`pane_col` are already
    /// hit-tested and translated into the log pane's own content area by
    /// the caller (`lib::dispatch`, the layer that knows the terminal's
    /// current geometry) — `AppState` itself never touches the screen.
    /// A drag start (`Down`) anchors a fresh selection at the clicked
    /// line, a `Drag` extends the cursor to it, and releasing (`Up`)
    /// copies the selection and returns to Normal, mirroring `y`. Silent
    /// outside `Mode::Copy`, and when the row does not land on a real
    /// buffer line (the truncation marker, or below the last line).
    pub fn handle_mouse(
        &mut self,
        kind: MouseEventKind,
        pane_height: usize,
        pane_row: usize,
        pane_col: usize,
    ) {
        if !matches!(self.mode, Mode::Copy(_)) {
            return;
        }
        let beam = self.current_beam_id();
        let Some(line) = self
            .logs
            .get(&beam)
            .and_then(|buffer| crate::copy::line_for_pane_row(buffer, pane_height, pane_row))
        else {
            return;
        };
        let Mode::Copy(mut copy) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("checked above")
        };
        match kind {
            MouseEventKind::Down(_) => {
                copy.anchor = (line, pane_col);
                copy.cursor = (line, pane_col);
                self.mode = Mode::Copy(copy);
            }
            MouseEventKind::Drag(_) => {
                copy.cursor = (line, pane_col);
                self.mode = Mode::Copy(copy);
            }
            MouseEventKind::Up(_) => {
                copy.cursor = (line, pane_col);
                if let Some(buffer) = self.logs.get(&beam) {
                    let text = copy.selected_text(buffer);
                    self.last_copy_result = Some(crate::copy::copy_to_clipboard(&text));
                }
                // stays Mode::Normal
            }
            _ => self.mode = Mode::Copy(copy),
        }
    }

    /// `n`: the Normal-mode binding that steps the *committed* search
    /// (`last_search`) to its next match, wrapping. A no-op when nothing
    /// has been committed yet.
    pub fn search_next(&mut self) {
        self.step_committed_search(SearchState::next);
    }

    /// `N`: same as `search_next`, the other way.
    pub fn search_previous(&mut self) {
        self.step_committed_search(SearchState::previous);
    }

    fn step_committed_search(&mut self, step: fn(&mut SearchState)) {
        let Some(mut committed) = self.last_search.take() else {
            return;
        };
        let selected = self.current_beam_id();
        if committed.beam != selected {
            // The selection has moved on since this search was
            // committed: the query is what the user typed, the beam is
            // what they are now looking at, so re-run it there rather
            // than stepping indices that describe a beam they have
            // since left.
            committed.beam = selected;
            self.recompute_search(&mut committed.search);
        }
        step(&mut committed.search);
        self.sync_search_scroll(&committed.search);
        self.last_search = Some(committed);
    }

    /// Whenever the selected beam's buffer gains a line while a search is
    /// active, its match set must keep up without waiting for the next
    /// keystroke — see the `BeamOutput` arm of `apply`.
    fn resync_active_search(&mut self) {
        if !matches!(self.mode, Mode::Search(_)) {
            return;
        }
        let Mode::Search(mut search) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("checked above")
        };
        self.recompute_search(&mut search);
        self.sync_search_scroll(&search);
        self.mode = Mode::Search(search);
    }

    /// Reruns the search against the selected beam's buffer — called
    /// whenever the query or the buffer's contents change, not on every
    /// keystroke (`search_next`/`search_previous` step the existing
    /// matches instead of rescanning them).
    fn recompute_search(&self, search: &mut SearchState) {
        self.recompute_search_against(&self.current_beam_id(), search);
    }

    /// Same as `recompute_search`, against an explicit beam rather than
    /// whichever one is currently selected — for invalidating a
    /// *committed* search whose beam a rerun just cleared, which need
    /// not be the beam on screen right now (see the `BeamStarted` /
    /// `BeamCached` arm of `apply`). A beam with no buffer yet (or
    /// whose buffer is gone) matches nothing, the same as an empty
    /// query would.
    fn recompute_search_against(&self, beam: &str, search: &mut SearchState) {
        match self.logs.get(beam) {
            Some(buffer) => search.update(buffer),
            None => {
                search.matches.clear();
                search.current = 0;
            }
        }
    }

    /// Translates `current_line` into a `Paused` offset on the selected
    /// beam's buffer, riding the log pane's existing Following/Paused
    /// scrolling rather than inventing a second mechanism for search to
    /// keep its current match on screen. No match (an empty query, or a
    /// query the buffer no longer contains) resumes `Following`: a
    /// search with nothing to show must not leave the pane pinned to a
    /// stale view.
    fn sync_search_scroll(&mut self, search: &SearchState) {
        let id = self.current_beam_id();
        let Some(buffer) = self.logs.get_mut(&id) else {
            return;
        };
        match search.current_line() {
            Some(line) => {
                let offset = buffer.len().saturating_sub(1).saturating_sub(line);
                buffer.follow_tail();
                buffer.scroll_up(offset);
            }
            None => buffer.follow_tail(),
        }
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
    use crossterm::event::KeyModifiers;
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
        assert_eq!(
            lines(&state, "build"),
            vec![("old line".to_string(), false)],
            "the first run's output is on screen until the rerun"
        );
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

    /// Each beam's output goes to its own buffer, and a replayed line
    /// stays marked as replayed — the log pane styles the two apart.
    #[test]
    fn output_lands_in_its_own_beams_buffer() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["codegen", "build"], &[]), now);
        state.apply(&RunEvent::BeamCached { id: id("codegen") }, now);
        state.apply(&replayed_output("codegen", "cached line"), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "first"), now);
        state.apply(&output("build", "second"), now);
        assert_eq!(
            lines(&state, "codegen"),
            vec![("cached line".to_string(), true)]
        );
        assert_eq!(
            lines(&state, "build"),
            vec![("first".to_string(), false), ("second".to_string(), false)]
        );
    }

    /// A `RunFinished` still draining behind the interrupt must not put
    /// an outcome back: Ctrl-C ended the session, it did not conclude it.
    #[test]
    fn an_event_draining_behind_an_interrupt_does_not_vouch() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.quit_via_interrupt();
        state.apply(&summary_event(&[]), now);
        assert_eq!(state.exit_outcome(), None);
        assert!(state.should_quit);
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

    /// `/`: a fresh session starts with nothing typed and nothing matched.
    #[test]
    fn entering_search_starts_with_an_empty_query() {
        let mut state = AppState::new("build", false);
        state.enter_search();
        let search = search_state(&state);
        assert_eq!(search.query, "");
        assert!(search.matches.is_empty());
    }

    /// Typing feeds the query, and the mode's `SearchState` picks up the
    /// selected beam's matches on every keystroke.
    #[test]
    fn typing_narrows_the_selected_beams_matches() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "Compiling api"), now);
        state.apply(&output("build", "warning: unused"), now);
        state.apply(&output("build", "Compiling core"), now);

        state.enter_search();
        for character in "compiling".chars() {
            state.handle_modal_key(char_key(character));
        }

        assert_eq!(search_state(&state).matches, vec![0, 2]);
    }

    /// `Enter` leaves search mode but stashes the query and its matches
    /// in `last_search`, which is what keeps the log pane highlighted.
    #[test]
    fn enter_leaves_search_mode_and_keeps_the_highlights() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "Compiling api"), now);

        state.enter_search();
        state.handle_modal_key(char_key('c'));
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(state.mode, Mode::Normal);
        let last_search = state.last_search.expect("Enter stashes the search");
        assert_eq!(last_search.search.query, "c");
        assert_eq!(last_search.search.matches, vec![0]);
    }

    /// `Esc` (the event loop's `leave_mode`) keeps the highlights the
    /// same way `Enter` does.
    #[test]
    fn leave_mode_stashes_search_into_last_search() {
        let mut state = AppState::new("build", false);
        state.enter_search();
        state.handle_modal_key(char_key('x'));
        state.leave_mode();

        assert_eq!(state.mode, Mode::Normal);
        assert_eq!(
            state
                .last_search
                .expect("Esc stashes the search")
                .search
                .query,
            "x"
        );
    }

    /// A fresh search session replaces whatever `last_search` was left
    /// behind by the previous one.
    #[test]
    fn a_new_search_session_starts_past_the_previous_ones_highlights() {
        let mut state = AppState::new("build", false);
        state.enter_search();
        state.handle_modal_key(char_key('x'));
        state.leave_mode();
        assert!(state.last_search.is_some());

        state.enter_search();
        assert_eq!(
            search_state(&state).query,
            "",
            "the new session starts empty"
        );
    }

    /// Backspace narrows the query back down, and the matches narrow
    /// with it on the very next keystroke.
    #[test]
    fn backspace_shrinks_the_query() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "Compiling api"), now);

        state.enter_search();
        for character in "Compilingx".chars() {
            state.handle_modal_key(char_key(character));
        }
        assert!(search_state(&state).matches.is_empty());

        state.handle_modal_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(search_state(&state).query, "Compiling");
        assert_eq!(search_state(&state).matches, vec![0]);
    }

    /// The vim/less split: `n`/`N` are Normal-mode bindings that step the
    /// *committed* search, so while a query is still being composed they
    /// are ordinary characters like any other — the query can contain
    /// them freely (`"running"`, `"warning"`, `"Compiling"`... are all
    /// common in build output).
    #[test]
    fn n_and_shift_n_are_ordinary_characters_while_composing() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "Compiling api"), now);
        state.apply(&output("build", "warning: unused"), now);
        state.apply(&output("build", "Compiling core"), now);

        state.enter_search();
        for character in "compiling".chars() {
            state.handle_modal_key(char_key(character));
        }

        assert_eq!(search_state(&state).query, "compiling");
        assert_eq!(search_state(&state).matches, vec![0, 2]);
    }

    /// `search_next`/`search_previous` (the `n`/`N` Normal-mode bindings)
    /// step the search `Enter` committed into `last_search`, wrapping.
    #[test]
    fn search_next_and_previous_step_the_committed_search() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "Compiling api"), now);
        state.apply(&output("build", "warning: unused"), now);
        state.apply(&output("build", "Compiling core"), now);

        state.enter_search();
        for character in "compiling".chars() {
            state.handle_modal_key(char_key(character));
        }
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(state.mode, Mode::Normal, "precondition");
        assert_eq!(
            state.last_search.as_ref().unwrap().search.current_line(),
            Some(0)
        );

        state.search_next();
        assert_eq!(
            state.last_search.as_ref().unwrap().search.current_line(),
            Some(2)
        );
        state.search_next(); // wraps
        assert_eq!(
            state.last_search.as_ref().unwrap().search.current_line(),
            Some(0)
        );
        state.search_previous();
        assert_eq!(
            state.last_search.as_ref().unwrap().search.current_line(),
            Some(2)
        );
    }

    /// Stepping with nothing committed must not panic.
    #[test]
    fn search_next_and_previous_are_no_ops_with_nothing_committed() {
        let mut state = AppState::new("build", false);
        state.search_next();
        state.search_previous();
        assert!(state.last_search.is_none());
    }

    /// A search committed on one beam must not step stale indices into
    /// whatever beam the user has since selected: `n` re-runs the same
    /// query against the newly selected beam's own buffer.
    #[test]
    fn search_next_re_runs_the_query_against_a_newly_selected_beam() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["codegen", "build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
        state.apply(&output("codegen", "line 0"), now);
        state.apply(&output("codegen", "ERROR in codegen"), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "unrelated line"), now);
        state.apply(&output("build", "another unrelated line"), now);

        // Commit a search for "error" while "codegen" (index 0) is
        // selected: it matches codegen's second line.
        state.select(0);
        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            state.last_search.as_ref().unwrap().beam,
            "codegen",
            "precondition"
        );

        // The user now looks at "build", which has no match at all.
        state.select(1);
        state.search_next();

        assert_eq!(
            state.last_search.as_ref().unwrap().beam,
            "build",
            "the committed search now tracks the beam actually on screen"
        );
        assert!(
            state
                .last_search
                .as_ref()
                .unwrap()
                .search
                .matches
                .is_empty(),
            "\"error\" matches nothing in build's own buffer"
        );
        assert!(
            matches!(
                state.logs.get("build").unwrap().scroll(),
                crate::logs::Scroll::Following
            ),
            "no match in the new beam resumes following rather than pinning a stale offset"
        );
    }

    /// Stepping into a beam that *does* match, after switching, lands on
    /// that beam's own first match rather than an index computed against
    /// the beam the search was originally run on.
    #[test]
    fn search_next_finds_the_new_beams_own_match_after_switching() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["codegen", "build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
        state.apply(&output("codegen", "ERROR in codegen"), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "line 0"), now);
        state.apply(&output("build", "ERROR in build"), now);

        state.select(0);
        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        state.select(1); // "build"
        state.search_next();

        assert_eq!(
            state.last_search.as_ref().unwrap().search.current_line(),
            Some(1),
            "build's own \"ERROR in build\" line, not codegen's"
        );
    }

    /// A rerun clears the beam's buffer; a search committed against the
    /// previous run's content must not leave `n`/`N` stepping into
    /// indices the cleared buffer no longer holds.
    #[test]
    fn a_rerun_invalidates_a_committed_search_on_the_same_beam() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "line 0"), now);
        state.apply(&output("build", "ERROR here"), now);

        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            state.last_search.as_ref().unwrap().search.matches,
            vec![1],
            "precondition"
        );

        // The beam reruns: BeamStarted clears its buffer.
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);

        assert!(
            state
                .last_search
                .as_ref()
                .unwrap()
                .search
                .matches
                .is_empty(),
            "the cleared buffer no longer holds line 1"
        );

        // Stepping must not panic, and must not pin a stale offset.
        state.search_next();
        assert!(
            state
                .last_search
                .as_ref()
                .unwrap()
                .search
                .current_line()
                .is_none(),
            "still nothing to step to"
        );
        assert!(matches!(
            state.logs.get("build").unwrap().scroll(),
            crate::logs::Scroll::Following
        ));
    }

    /// A still-running beam's output must not go stale just because the
    /// user has not typed since it arrived: `apply`'s `BeamOutput` arm
    /// re-triggers the active search the same way a keystroke would.
    #[test]
    fn output_arriving_during_an_active_search_recomputes_it() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "line 0"), now);

        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }
        assert!(
            search_state(&state).matches.is_empty(),
            "precondition: no match yet"
        );

        state.apply(&output("build", "ERROR: build failed"), now);

        assert_eq!(
            search_state(&state).matches,
            vec![1],
            "the new line is picked up without another keystroke"
        );
        assert_eq!(search_state(&state).current_line(), Some(1));
    }

    /// Output for a beam that is not selected must not disturb the
    /// active search — it is not the buffer being searched.
    #[test]
    fn output_for_an_unselected_beam_does_not_recompute_the_search() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["codegen", "build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.select(1); // "build"

        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }

        state.apply(&output("codegen", "ERROR: not the searched beam"), now);

        assert!(search_state(&state).matches.is_empty());
    }

    /// Backspacing down to an empty query (no matches left) must resume
    /// following rather than leaving the pane pinned to a stale view.
    #[test]
    fn a_query_with_no_matches_resumes_following() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "ERROR here"), now);
        for index in 0..50 {
            state.apply(&output("build", &format!("line {index}")), now);
        }

        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }
        assert!(matches!(
            state.logs.get("build").unwrap().scroll(),
            crate::logs::Scroll::Paused { .. }
        ));

        for _ in 0.."error".len() {
            state.handle_modal_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }

        assert!(matches!(
            state.logs.get("build").unwrap().scroll(),
            crate::logs::Scroll::Following
        ));
    }

    /// The log pane rides the buffer's own Following/Paused scrolling:
    /// searching for a match well above the tail pauses the buffer so
    /// that match is the last line its view shows.
    #[test]
    fn searching_scrolls_the_buffer_to_the_current_match() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "ERROR here"), now);
        for index in 0..50 {
            state.apply(&output("build", &format!("line {index}")), now);
        }

        state.enter_search();
        for character in "error".chars() {
            state.handle_modal_key(char_key(character));
        }

        assert_eq!(search_state(&state).current_line(), Some(0));
        let buffer = state.logs.get("build").unwrap();
        let view = buffer.view(5);
        assert_eq!(
            view.last().map(String::as_str),
            Some("ERROR here"),
            "the match is scrolled to the bottom of its view"
        );
    }

    fn search_state(state: &AppState) -> &SearchState {
        match &state.mode {
            Mode::Search(search) => search,
            other => panic!("expected Mode::Search, got {other:?}"),
        }
    }

    fn char_key(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
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

    fn replayed_output(beam: &str, text: &str) -> RunEvent {
        match output(beam, text) {
            RunEvent::BeamOutput { id, line, .. } => RunEvent::BeamOutput {
                id,
                line,
                replayed: true,
            },
            other => other,
        }
    }

    /// What a beam's buffer holds, as `(text, replayed)` pairs.
    fn lines(state: &AppState, beam: &str) -> Vec<(String, bool)> {
        state
            .logs
            .get(beam)
            .expect("the beam has a buffer")
            .lines()
            .map(|line| (line.text.clone(), line.replayed))
            .collect()
    }
}
