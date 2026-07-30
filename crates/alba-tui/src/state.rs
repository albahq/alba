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
use crate::graph::GraphState;
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
    Graph(GraphState),
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
    /// The text `y` or a mouse release last selected, waiting for the
    /// composition root (`lib::run`) to actually attempt the clipboard
    /// write — `AppState` never performs that itself (it would make
    /// this a pure fold no longer, and every unit test that presses `y`
    /// would reach a real terminal/clipboard). Taken, not just read, by
    /// whoever drains it: it is a one-shot request, not standing state.
    pub pending_copy: Option<String>,
    /// What that attempt reported, and until when the bottom bar keeps
    /// showing it (see `CopyResult::is_visible`).
    pub last_copy_result: Option<CopyResult>,
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
    /// The log pane's current content height — refreshed by
    /// `lib::dispatch` before every action, since it is the one piece of
    /// screen geometry copy mode's keyboard flow needs (`v`'s anchor,
    /// keeping the cursor's line inside what the pane actually shows)
    /// and `AppState` otherwise has no way to learn, never touching the
    /// terminal itself.
    pane_height: usize,
}

/// How long a copy result stays in the bottom bar once shown — long
/// enough to survive the several redraws that can land inside it (the
/// 80ms tick, a beam still flooding its own output), rather than the
/// single frame a literal reading of the brief's "for one draw" would
/// give it, gone well under a blink. The plan owner's ruling on that
/// ambiguity.
const COPY_RESULT_VISIBLE: Duration = Duration::from_secs(2);

/// What the last copy attempt reported, and until when the bottom bar
/// keeps showing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyResult {
    pub message: &'static str,
    expires_at: Instant,
}

impl CopyResult {
    fn new(message: &'static str, now: Instant) -> Self {
        Self {
            message,
            expires_at: now + COPY_RESULT_VISIBLE,
        }
    }

    /// Whether the bottom bar should still be showing this at `now`.
    pub fn is_visible(&self, now: Instant) -> bool {
        now < self.expires_at
    }
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
            pending_copy: None,
            last_copy_result: None,
            last_summary: None,
            should_quit: false,
            outcome: None,
            user_cancelled: false,
            waiting_seen: false,
            pane_height: 0,
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

    /// The selected beam's id, or an empty string when there is none.
    /// Search is tied to a specific beam's own buffer even while parked
    /// (there is nothing there to search anyway), so it uses this
    /// directly; anything that should instead follow whatever the pane
    /// is *currently showing* — parked or not — uses `displayed_log_key`.
    fn current_beam_id(&self) -> String {
        self.selected_beam()
            .map(|row| row.id.clone())
            .unwrap_or_default()
    }

    /// The log key the pane actually shows right now: the diagnostic's
    /// pseudo-beam while `Phase::Parked` — the diagnostic is the only
    /// thing on screen then, and precisely what copy mode's `v`/`y` must
    /// address — the selected beam otherwise. `ui/logpane.rs`'s `draw`
    /// and every copy-mode entry point resolve their buffer through this
    /// same method, so the highlight and what `y` actually copies can
    /// never disagree about what is on screen.
    pub fn displayed_log_key(&self) -> String {
        if matches!(self.phase, Phase::Parked) {
            DIAGNOSTIC_LOG.to_string()
        } else {
            self.current_beam_id()
        }
    }

    /// Refreshed by `lib::dispatch` before every action — the one piece
    /// of screen geometry copy mode's keyboard flow needs and `AppState`
    /// has no other way to learn, since it never touches the terminal.
    pub fn set_pane_height(&mut self, height: usize) {
        self.pane_height = height;
    }

    /// Where a copy attempt's result lands once the composition root
    /// (`lib::run`) has actually performed it — `AppState` never
    /// touches the clipboard itself; `pending_copy` is what `y` or a
    /// mouse release leaves behind to ask for that, and this is where
    /// the answer comes back for the bottom bar to show.
    pub fn record_copy_result(&mut self, message: &'static str, now: Instant) {
        self.last_copy_result = Some(CopyResult::new(message, now));
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
    /// current scroll window shows at its own top — of whatever buffer
    /// is actually on screen (`displayed_log_key`), the diagnostic's
    /// included, while the project is parked.
    pub fn enter_copy(&mut self) {
        let beam = self.displayed_log_key();
        let top_line = self
            .logs
            .get(&beam)
            .map(|buffer| crate::copy::top_visible_line(buffer, self.pane_height))
            .unwrap_or(0);
        self.mode = Mode::Copy(CopyState::new_at(top_line));
    }

    /// `g`: opens the graph view focused on whichever beam is already
    /// selected, so the reader lands on the node they were just looking
    /// at rather than always starting at layer 0.
    pub fn enter_graph(&mut self) {
        self.mode = Mode::Graph(GraphState::new(self.selected));
    }

    /// `?`: opens the help overlay. Unlike Search/Copy/Graph it carries
    /// no payload of its own — the keymap it lists is fixed, not a
    /// function of anything in `AppState` — so there is nothing to
    /// compute here beyond the mode switch itself.
    pub fn enter_help(&mut self) {
        self.mode = Mode::Help;
    }

    /// `Esc`: leaves whatever modal mode is active. Search stashes its
    /// state into `last_search` first, tagged with the beam it ran
    /// against — the only modal mode whose payload is worth keeping
    /// around after leaving. Copy's selection is simply discarded:
    /// abandoning it without copying is exactly what `Esc` is for.
    pub fn leave_mode(&mut self) {
        if let Mode::Search(search) = std::mem::replace(&mut self.mode, Mode::Normal) {
            self.last_search = Some(CommittedSearch {
                beam: self.current_beam_id(),
                search,
            });
        }
    }

    /// The modal keymap's entry point: routes a key event to whichever
    /// mode is active. Help has no arm here: its only two live keys
    /// (`Esc`, `?`) both resolve to `Action::LeaveMode` in `input.rs`
    /// itself and never reach this method — every other key while help
    /// is showing is inert, which is exactly what the wildcard arm gives
    /// it for free.
    pub fn handle_modal_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Search(_) => self.handle_search_key(key),
            Mode::Copy(_) => self.handle_copy_key(key),
            Mode::Graph(_) => self.handle_graph_key(key),
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
    /// (starting a fresh span from there), and `y` stages the selected
    /// text in `pending_copy` and returns to Normal — the composition
    /// root (`lib::run`) is what actually attempts the clipboard write;
    /// `AppState` never does, so it stays a pure fold and `y` stays
    /// assertable on `pending_copy` alone. `Esc` leaving without copying
    /// is `leave_mode`'s job, same as every other modal mode.
    fn handle_copy_key(&mut self, key: KeyEvent) {
        let Mode::Copy(mut copy) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("handle_modal_key only calls this while Mode::Copy is active")
        };
        match key.code {
            KeyCode::Char('v') => copy.anchor = copy.cursor,
            KeyCode::Char('y') => {
                let beam = self.displayed_log_key();
                if let Some(buffer) = self.logs.get(&beam) {
                    self.pending_copy = Some(copy.selected_text(buffer));
                }
                return; // stays Mode::Normal, already replaced above
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_copy_cursor(&mut copy, -1, 0),
            KeyCode::Down | KeyCode::Char('j') => self.move_copy_cursor(&mut copy, 1, 0),
            KeyCode::Left | KeyCode::Char('h') => self.move_copy_cursor(&mut copy, 0, -1),
            KeyCode::Right | KeyCode::Char('l') => self.move_copy_cursor(&mut copy, 0, 1),
            _ => {}
        }
        self.sync_copy_scroll(copy.cursor.0);
        self.mode = Mode::Copy(copy);
    }

    /// Graph mode's own keymap: arrows move the focus (`GraphState::navigate`),
    /// and `Enter` commits the focused beam as the selection and returns
    /// to Normal. The layering is recomputed fresh from `self.beams`/
    /// `self.edges` on every keystroke rather than cached alongside the
    /// mode — the DAG is fixed for the run in flight, so this costs
    /// nothing worth avoiding, and it rules out the layering and the beam
    /// table it describes ever drifting apart. `Esc` leaving without
    /// changing the selection is `leave_mode`'s job, same as every other
    /// modal mode: it already does the right thing here (the mode is
    /// simply dropped, `self.selected` untouched) without a special case.
    fn handle_graph_key(&mut self, key: KeyEvent) {
        let Mode::Graph(mut graph) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("handle_modal_key only calls this while Mode::Graph is active")
        };
        match key.code {
            KeyCode::Enter => {
                self.select(graph.focused);
                return; // stays Mode::Normal, already replaced above
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                let layers = crate::graph::layers(self.beams.len(), &self.edges);
                graph.navigate(key.code, &layers);
            }
            _ => {}
        }
        self.mode = Mode::Graph(graph);
    }

    fn move_copy_cursor(&self, copy: &mut CopyState, dl: isize, dc: isize) {
        let beam = self.displayed_log_key();
        if let Some(buffer) = self.logs.get(&beam) {
            copy.move_cursor(dl, dc, buffer);
        }
    }

    /// Keeps the log pane's own scroll window following the cursor after
    /// it moves — copy mode's equivalent of what `sync_search_scroll`
    /// already does for the current match. `enter_copy` anchors at the
    /// pane's *top* visible line, so the very first `k` would otherwise
    /// put the cursor one line above what the window shows, silently
    /// growing the selection into rows the reader cannot see (and the
    /// same downward, once the buffer is `Paused`); this brings that
    /// line back into view instead, at whichever edge the cursor left
    /// from.
    fn sync_copy_scroll(&mut self, cursor_line: usize) {
        let pane_height = self.pane_height;
        if pane_height == 0 {
            return;
        }
        let beam = self.displayed_log_key();
        let Some(buffer) = self.logs.get_mut(&beam) else {
            return;
        };
        if buffer.is_empty() {
            return;
        }
        let len = buffer.len();
        let (start, end) = crate::copy::scroll_window(buffer, pane_height);
        if cursor_line < start {
            // Bring the cursor to the top of the view.
            let target_offset = len.saturating_sub(cursor_line + pane_height);
            buffer.follow_tail();
            // `scroll_up` unconditionally switches to `Paused` even for
            // `by: 0` (`LogBuffer::scroll_up`), so calling it with an
            // offset of exactly 0 would leave the buffer `Paused {
            // offset: 0 }` — indistinguishable from `Following` right
            // now, but *not* auto-following: `push` only increments a
            // `Paused` offset, so a beam that keeps producing output
            // after the cursor reaches the tail would drift one line
            // behind it per pushed line. `follow_tail` alone already is
            // the offset-0 case; skip the redundant (and harmful) call.
            if target_offset > 0 {
                buffer.scroll_up(target_offset);
            }
        } else if cursor_line >= end {
            // Bring the cursor to the bottom of the view — same
            // reasoning as above: an offset of 0 here means the cursor
            // reached the true tail, which must stay `Following`.
            let target_offset = len.saturating_sub(cursor_line + 1);
            buffer.follow_tail();
            if target_offset > 0 {
                buffer.scroll_up(target_offset);
            }
        }
    }

    /// Copy mode's drag entry point: `pane_row`/`pane_col` are already
    /// hit-tested and translated into the log pane's own content area by
    /// the caller (`lib::dispatch_mouse`, the layer that knows the
    /// terminal's current geometry) — `AppState` itself never touches
    /// the screen. `pane_col` is the click's screen column used
    /// directly as a character index: correct for ASCII content, but a
    /// wide character or a tab before the click shifts the selection
    /// from the pointer by that column's width (no panic risk either
    /// way — `CopyState`'s char-index slicing always clamps).
    ///
    /// `Down` anchors a fresh selection at the clicked line; `Drag`
    /// extends the cursor to it. A release (`Up`) is not handled here —
    /// `finish_copy` is, and `lib::dispatch_mouse` calls it directly
    /// regardless of where the release landed, so dragging off the
    /// bottom of the buffer (or into the tree pane) and letting go there
    /// still finishes the copy. Silent outside `Mode::Copy`, and when
    /// the row does not land on a real buffer line (the truncation
    /// marker, or below the last line).
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
        if !matches!(kind, MouseEventKind::Down(_) | MouseEventKind::Drag(_)) {
            return;
        }
        let beam = self.displayed_log_key();
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
        copy.cursor = (line, pane_col);
        if matches!(kind, MouseEventKind::Down(_)) {
            copy.anchor = (line, pane_col);
        }
        self.mode = Mode::Copy(copy);
    }

    /// Finishes copy mode's selection wherever it currently is: stages
    /// the selected text in `pending_copy` and returns to Normal. Called
    /// by `lib::dispatch_mouse` for every mouse release while copying,
    /// whether or not the release itself landed inside the log pane —
    /// the selection already has an anchor and a cursor from wherever
    /// the drag last touched the pane, so ending the gesture off it
    /// (past the last log line, or into the tree) must not strand the
    /// user mid-selection with nothing copied. A no-op outside
    /// `Mode::Copy`.
    pub fn finish_copy(&mut self) {
        if !matches!(self.mode, Mode::Copy(_)) {
            return;
        }
        let Mode::Copy(copy) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            unreachable!("checked above")
        };
        let beam = self.displayed_log_key();
        if let Some(buffer) = self.logs.get(&beam) {
            self.pending_copy = Some(copy.selected_text(buffer));
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

    fn cursor(state: &AppState) -> (usize, usize) {
        match &state.mode {
            Mode::Copy(copy) => copy.cursor,
            other => panic!("expected Mode::Copy, got {other:?}"),
        }
    }

    /// The keyboard wiring itself: `hjkl` and the arrows move the cursor
    /// in the direction their name promises, not a swapped one — the
    /// exact place a mixed-up delta would hide.
    #[test]
    fn copy_mode_movement_keys_move_the_expected_direction() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        for line in ["alpha", "bravo", "charlie"] {
            state.apply(&output("build", line), now);
        }
        state.set_pane_height(10);
        state.enter_copy(); // anchors at (0, 0): all 3 lines fit, following

        state.handle_modal_key(char_key('l'));
        assert_eq!(cursor(&state), (0, 1), "l moves the column forward");
        state.handle_modal_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(cursor(&state), (0, 2), "Right moves the column forward");
        state.handle_modal_key(char_key('h'));
        assert_eq!(cursor(&state), (0, 1), "h moves the column back");
        state.handle_modal_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(cursor(&state), (0, 0), "Left moves the column back");

        state.handle_modal_key(char_key('j'));
        assert_eq!(cursor(&state), (1, 0), "j moves to the next line");
        state.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(cursor(&state), (2, 0), "Down moves to the next line");
        state.handle_modal_key(char_key('k'));
        assert_eq!(cursor(&state), (1, 0), "k moves to the previous line");
        state.handle_modal_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(cursor(&state), (0, 0), "Up moves to the previous line");
    }

    /// `v` starts a fresh span from wherever the cursor currently sits,
    /// rather than resetting it back to the pane's top line.
    #[test]
    fn v_re_anchors_the_selection_at_the_cursor() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        for line in ["alpha", "bravo", "charlie"] {
            state.apply(&output("build", line), now);
        }
        state.set_pane_height(10);
        state.enter_copy();
        state.handle_modal_key(char_key('j'));
        state.handle_modal_key(char_key('l'));
        assert_eq!(cursor(&state), (1, 1), "precondition");

        state.handle_modal_key(char_key('v'));

        match &state.mode {
            Mode::Copy(copy) => {
                assert_eq!(copy.anchor, (1, 1), "re-anchored at the cursor");
                assert_eq!(copy.cursor, (1, 1));
            }
            other => panic!("expected Mode::Copy, got {other:?}"),
        }
    }

    /// `y` stages the selection in `pending_copy` and returns to Normal
    /// — `AppState` itself never attempts the clipboard write, which is
    /// exactly what makes this assertable without reaching a real
    /// terminal or clipboard from a unit test.
    #[test]
    fn y_stages_the_selection_in_pending_copy_without_touching_the_clipboard() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        state.apply(&output("build", "alpha"), now);
        state.apply(&output("build", "bravo"), now);
        state.set_pane_height(10);
        state.enter_copy(); // anchors at (0, 0)
        state.handle_modal_key(char_key('l')); // cursor -> (0, 1)

        state.handle_modal_key(char_key('y'));

        assert_eq!(state.mode, Mode::Normal);
        assert_eq!(state.pending_copy.as_deref(), Some("al"));
        assert!(
            state.last_copy_result.is_none(),
            "AppState never performs the clipboard write itself"
        );
    }

    /// The equivalent of `sync_search_scroll` for copy mode: moving the
    /// cursor above or below the pane's current scroll window brings
    /// that line back into view rather than silently growing the
    /// selection into rows the reader cannot see. `enter_copy` anchors
    /// at the pane's *top* visible line, so the very first `k` is
    /// exactly where this would otherwise go unnoticed.
    #[test]
    fn copy_mode_movement_scrolls_the_pane_to_follow_the_cursor() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.apply(&run_started("build", &["build"], &[]), now);
        state.apply(&RunEvent::BeamStarted { id: id("build") }, now);
        for index in 0..30 {
            state.apply(&output("build", &format!("line {index}")), now);
        }
        state.set_pane_height(5);

        state.enter_copy();
        assert_eq!(
            cursor(&state),
            (25, 0),
            "anchored at the pane's top visible line"
        );

        // `k` moves the cursor one line above the current top of the
        // view; the pane must scroll up by one to keep it visible.
        state.handle_modal_key(char_key('k'));
        assert_eq!(cursor(&state), (24, 0));
        assert_eq!(
            state.logs.get("build").unwrap().view(5)[0],
            "line 24",
            "the cursor's line is now the top of the view"
        );

        // Stepping back down past the bottom of that (now paused) view
        // brings the cursor to the tail, and the view follows it there.
        for _ in 0..5 {
            state.handle_modal_key(char_key('j'));
        }
        assert_eq!(cursor(&state), (29, 0), "clamped to the last line");
        let buffer = state.logs.get("build").unwrap();
        let view = buffer.view(5);
        assert_eq!(
            view.last().map(String::as_str),
            Some("line 29"),
            "the view follows the cursor back down to the tail"
        );
        // Reaching the tail must resume true `Following`, not `Paused {
        // offset: 0 }` — the two render the same view right now, but
        // only `Following` keeps auto-following further output (see
        // `sync_copy_scroll`'s doc comment on why this matters).
        assert!(
            matches!(buffer.scroll(), crate::logs::Scroll::Following),
            "the tail must be true Following, not Paused {{ offset: 0 }}"
        );

        // Regression check: a beam that keeps producing output after
        // the cursor reaches the tail must have the pane keep following
        // it, not silently drift one line behind per pushed line.
        state.apply(&output("build", "line 30"), Instant::now());
        let view_after_push = state.logs.get("build").unwrap().view(5);
        assert_eq!(
            view_after_push.last().map(String::as_str),
            Some("line 30"),
            "the view keeps following the tail after the cursor reached it"
        );
    }

    /// Parking the project makes the diagnostic the only thing on
    /// screen (`ui/logpane.rs` renders it under `DIAGNOSTIC_LOG`); copy
    /// mode must resolve that very same buffer — not the selected
    /// beam's, which may not even hold the diagnostic at all — or the
    /// highlight and what `y` actually copies would disagree about what
    /// is on screen.
    #[test]
    fn copy_mode_addresses_the_diagnostic_while_parked() {
        let mut state = AppState::new("build", true);
        let now = Instant::now();
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\nhelp: fix it\n".to_string(),
            },
            now,
        );
        state.set_pane_height(10);

        state.enter_copy();
        assert_eq!(cursor(&state), (0, 0));
        state.handle_modal_key(char_key('l'));
        state.handle_modal_key(char_key('y'));

        assert_eq!(
            state.pending_copy.as_deref(),
            Some("er"),
            "copied from the diagnostic buffer, not an empty selected-beam one"
        );
    }

    /// `g`: opens the graph view already focused on the selected beam,
    /// rather than always starting at layer 0.
    #[test]
    fn entering_graph_mode_focuses_the_selected_beam() {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["codegen", "build"], &[("build", "codegen")]),
            Instant::now(),
        );
        state.select(1);

        state.enter_graph();

        match &state.mode {
            Mode::Graph(graph) => assert_eq!(graph.focused, 1),
            other => panic!("expected Mode::Graph, got {other:?}"),
        }
    }

    /// `Enter` in graph mode commits the focused beam as the selection
    /// and returns to Normal — `Esc` (via `leave_mode`, exercised
    /// separately) must leave the selection untouched instead.
    #[test]
    fn enter_in_graph_mode_selects_the_focused_beam_and_leaves() {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["codegen", "build"], &[("build", "codegen")]),
            Instant::now(),
        );
        state.select(0); // "codegen"

        state.enter_graph();
        state.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(state.mode, Mode::Normal);
        assert_eq!(
            state.selected_beam().map(|row| row.id.as_str()),
            Some("build"),
            "the focus arrows moved to before Enter committed it"
        );
    }

    /// `Esc` leaves graph mode the same generic way every other modal
    /// mode does (`leave_mode`): the selection stays exactly what it was
    /// before the graph view was ever opened.
    #[test]
    fn esc_leaves_graph_mode_without_changing_the_selection() {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["codegen", "build"], &[("build", "codegen")]),
            Instant::now(),
        );
        state.select(0);

        state.enter_graph();
        state.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        state.leave_mode();

        assert_eq!(state.mode, Mode::Normal);
        assert_eq!(
            state.selected_beam().map(|row| row.id.as_str()),
            Some("codegen"),
            "Esc must not carry the arrow-moved focus into the selection"
        );
    }

    /// Arrow keys in graph mode move the focus via `GraphState::navigate`,
    /// fed the layering freshly computed from the current beams/edges.
    #[test]
    fn arrow_keys_in_graph_mode_move_the_focus() {
        let mut state = AppState::new("build", false);
        state.apply(
            &run_started("build", &["codegen", "build"], &[("build", "codegen")]),
            Instant::now(),
        );
        state.select(0); // "codegen", layer 0

        state.enter_graph();
        state.handle_modal_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        match &state.mode {
            Mode::Graph(graph) => assert_eq!(graph.focused, 1, "crossed down to \"build\""),
            other => panic!("expected Mode::Graph, got {other:?}"),
        }
    }

    /// `?`: opens help. `Esc` (via `leave_mode`, exercised everywhere
    /// else already) closes it the same generic way it closes every
    /// other modal mode — help carries no payload for `leave_mode` to
    /// have to stash anywhere.
    #[test]
    fn entering_help_opens_it_and_esc_closes_it() {
        let mut state = AppState::new("build", false);

        state.enter_help();
        assert_eq!(state.mode, Mode::Help);

        state.leave_mode();
        assert_eq!(state.mode, Mode::Normal);
    }

    /// `finish_copy` is a no-op outside `Mode::Copy` — the mouse
    /// release path calls it unconditionally, so it must not do
    /// anything to, say, a session still in `Mode::Normal`.
    #[test]
    fn finish_copy_outside_copy_mode_is_a_no_op() {
        let mut state = AppState::new("build", false);
        state.finish_copy();
        assert_eq!(state.mode, Mode::Normal);
        assert!(state.pending_copy.is_none());
    }

    /// `CopyResult` governs the bottom bar's own timing: visible right
    /// away and partway through its window, expired well past it — the
    /// plan owner's two-second ruling on the brief's "for one draw".
    #[test]
    fn copy_result_is_visible_until_its_deadline_then_not() {
        let now = Instant::now();
        let mut state = AppState::new("build", false);
        state.record_copy_result("copied (OSC 52)", now);

        let result = state.last_copy_result.expect("a result was recorded");
        assert_eq!(result.message, "copied (OSC 52)");
        assert!(result.is_visible(now), "visible right when it lands");
        assert!(
            result.is_visible(now + Duration::from_secs(1)),
            "still visible partway through the window"
        );
        assert!(
            !result.is_visible(now + Duration::from_secs(3)),
            "expired well past the two-second window"
        );
    }
}
