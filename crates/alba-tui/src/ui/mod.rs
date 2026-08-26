//! The whole-frame layout: an outer border carrying the header and
//! bottom bar, a vertical divider between the tree and log panes — the
//! interactive mirror of the CLI's headless renderers (see
//! `alba-cli/src/render/`), but a pure function of [`AppState`] rather
//! than a stream consumer.
//!
//! Nothing here samples a clock or touches the terminal: `now` arrives
//! as an argument, so a redraw is a deterministic function of its
//! inputs and a snapshot test owns the clock. See the spec's Layout
//! section for the mockup this module renders — the outer frame, the
//! divider, and the header/bottom-bar text sitting in the border are
//! all drawn exactly as that mockup shows them.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::state::{AppState, Mode};

mod footer;
mod graphpane;
mod header;
mod help;
mod logpane;
mod theme;
mod tree;

/// Below this floor, the tree and log panes have no room left to mean
/// anything, so the whole layout gives way to one message instead of
/// drawing a garbled screen (spec: "terminal too small", roughly 40x12).
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 12;

/// The tree pane's fixed content width, measured inside the frame's
/// outer border and the divider that separates it from the log pane.
/// The task brief's Interfaces line calls for "left tree pane (30
/// columns)" — that line, not a character count taken from the spec's
/// hand-drawn ASCII mockup (which comes out to 28 by literal count), is
/// the authority for this number.
const TREE_WIDTH: u16 = 30;

pub fn draw(frame: &mut Frame, state: &AppState, now: Instant) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new("terminal too small (need at least 40x12)").alignment(Alignment::Center),
            area,
        );
        return;
    }

    // The outer frame carries the header and the bottom bar inside its
    // own border, exactly as the spec's mockup draws them
    // (`┌─ alba · run build ── ... ─┐` / `└─ q quit · ... ─┘`): a
    // leading `─ ` and trailing ` ` are baked into the title itself so
    // the block's own border fill supplies the rest of the dashes,
    // wrapped around the header's and the bar's own spans rather than
    // their plain text, so the colour underneath survives into the
    // border's title.
    let outer = Block::bordered()
        .title_top(framed_title(header::line(state, now)))
        .title_bottom(framed_title(theme::bar_line(
            &bottom_bar(state, now),
            state.colour,
        )));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Inside the frame, top to bottom: the body (the two panes, or the
    // graph), the junction line that closes the panes' columns, and the
    // footer carrying the run. The bottom edge below them is the outer
    // block's own.
    let rows = Layout::vertical([
        Constraint::Min(0),    // body
        Constraint::Length(1), // junction: ├───┴───┤
        Constraint::Length(1), // footer: counts, bar or outcome
    ])
    .split(inner);
    let (body, junction, footer) = (rows[0], rows[1], rows[2]);

    footer::draw(frame, footer, state, now);

    // Graph mode replaces the body entirely — no tree, no log pane, no
    // divider between them, and no junction closing columns that are
    // not there: the graph gets the junction's row too. The footer stays:
    // the run is still the run while the reader looks at its graph.
    if let Mode::Graph(graph) = &state.mode {
        let body = Rect {
            height: body.height + junction.height,
            ..body
        };
        graphpane::draw(frame, body, state, graph);
        return;
    }

    // The tree pane's region includes its own right border, which is
    // the divider between it and the log pane — one column wider than
    // the tree's actual content width.
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(body);
    let divider = Block::new().borders(Borders::RIGHT);
    let tree_area = divider.inner(panes[0]);
    frame.render_widget(divider, panes[0]);

    tree::draw(frame, tree_area, state, now);
    logpane::draw(frame, panes[1], state);
    draw_junction(frame, area, junction);

    // Help is an overlay, not a replacement: the tree and log panes stay
    // drawn underneath it (dimmed), unlike graph mode's own early return
    // above, which takes over the body entirely instead.
    if matches!(state.mode, Mode::Help) {
        help::draw(frame, inner);
    }
}

/// The line closing the two panes' columns: `─` across the frame's
/// inside, `┴` where the divider lands on it, and the outer border's
/// own `│` on either side turned into `├`/`┤`. Those three cells are
/// written by hand: `Block` draws one rectangle's edges, and this row
/// is where three of them meet.
fn draw_junction(frame: &mut Frame, frame_area: Rect, junction: Rect) {
    frame.render_widget(
        Paragraph::new("─".repeat(junction.width as usize)),
        junction,
    );
    let buffer = frame.buffer_mut();
    buffer[(frame_area.x, junction.y)].set_symbol("├");
    buffer[(junction.x + TREE_WIDTH, junction.y)].set_symbol("┴");
    buffer[(frame_area.right() - 1, junction.y)].set_symbol("┤");
}

/// Wraps a header or bottom-bar line in the border's own `─ ... ` frame,
/// spans and all, so a coloured span inside it (the progress bar, the
/// outcome, a bold key) survives into the block's title rather than
/// being flattened to plain text first.
fn framed_title(content: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("─ ")];
    spans.extend(content.spans);
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// The always-available actions for the current mode.
///
/// A copy just made (`AppState::last_copy_result`) takes over the bar
/// entirely, regardless of mode, for as long as `CopyResult::is_visible`
/// says — by the time it is set, `y` (or a mouse release) has already
/// put the session back in `Mode::Normal`, so there is nothing else
/// worth advertising while it is shown. That window is a couple of
/// seconds rather than the single frame a literal "for one draw" would
/// give it: at the 80ms tick, or with a beam still flooding output, a
/// one-draw message would be gone well under a blink.
///
/// The Normal bar names the six actions a first-time reader needs, not
/// the whole keymap: `f`, `t`, `n`/`N`, `g`, and `v` live in the help
/// overlay (`?`) instead, which the bar itself points at. `help.rs`'s
/// `keymap_lines` is where all of them are listed.
fn bottom_bar(state: &AppState, now: Instant) -> String {
    if let Some(result) = &state.last_copy_result
        && result.is_visible(now)
    {
        return format!("{} · q quit", result.message);
    }
    match &state.mode {
        Mode::Normal => "q quit · r rerun · c cancel · w watch · / search · ? help".to_string(),
        Mode::Search(search) => {
            let total = search.matches.len();
            let current = if total == 0 { 0 } else { search.current + 1 };
            format!(
                "/{} · {current}/{total} · Enter commit · Esc cancel",
                search.query
            )
        }
        // Copy's, Graph's, and Help's text is the same constant
        // `help.rs`'s own overlay lists for these three modes
        // (`COPY_KEYS`/`GRAPH_KEYS`/`HELP_KEYS`) — one spelling of each,
        // not two hand-copied literals the overlay and this bar could
        // silently drift apart from each other.
        Mode::Copy(_) => help::COPY_KEYS.to_string(),
        Mode::Graph(_) => help::GRAPH_KEYS.to_string(),
        // While help is showing, only `Esc`/`?` (both close it) and
        // Ctrl-C do anything at all — `q` is swallowed as a plain
        // character the same as every other key `handle_modal_key`
        // doesn't recognize for this mode (see its own doc comment).
        Mode::Help => help::HELP_KEYS.to_string(),
    }
}

/// How long something took, as one decimal of a second (`4.1s`) — the
/// same shape as `alba-cli`'s `format_duration`, kept as its own copy
/// here because the dependency direction (`alba-cli` → `alba-tui`) runs
/// the wrong way for this crate to import it.
fn format_duration(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

/// The log pane's own content rectangle — inside the outer border, past
/// the tree pane and its divider, and inside the title row, and above
/// the junction and footer rows `logpane::draw` reserves — for a
/// terminal of `width` × `height` cells. `None` below the too-small
/// floor, where `draw` paints nothing but its one message and there is
/// no pane to hit-test against.
///
/// Copy mode's mouse handling (`lib::dispatch_mouse`) asks this rather
/// than re-deriving the layout its own way, so a click can never drift
/// out of sync with what `draw` actually painted: both go through the
/// exact same `Layout` calls.
pub fn log_pane_content_area(width: u16, height: u16) -> Option<Rect> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }
    let inner = Block::bordered().inner(Rect::new(0, 0, width, height));
    let rows = Layout::vertical([
        Constraint::Min(0),    // body
        Constraint::Length(1), // junction
        Constraint::Length(1), // footer
    ])
    .split(inner);
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(rows[0]);
    let pane = Layout::vertical([
        Constraint::Length(1), // "LOGS" title row
        Constraint::Min(0),    // output
    ])
    .split(panes[1]);
    Some(pane[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_pane_content_area_sits_past_the_tree_and_its_borders() {
        // Outer border: 1 cell each side. Tree pane + divider: 31
        // columns. Title row: 1 line. Junction and footer: 2 lines.
        let area = log_pane_content_area(80, 24).expect("80x24 clears the floor");
        assert_eq!(area, Rect::new(32, 2, 47, 19));
    }

    #[test]
    fn log_pane_content_area_is_none_below_the_floor() {
        assert_eq!(log_pane_content_area(MIN_WIDTH - 1, 24), None);
        assert_eq!(log_pane_content_area(80, MIN_HEIGHT - 1), None);
    }

    /// The junction row is where three block edges meet, and its three
    /// special cells are written by hand, so this pins them: `├` on the
    /// left edge, `┴` at the divider's foot, `┤` on the right edge.
    #[test]
    fn the_junction_closes_both_columns() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let state = AppState::new("build", false);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, &state, Instant::now()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Rows at 80x24: 23 is the bottom edge, 22 the footer, 21 the
        // junction, 20 the last body row.
        assert_eq!(buffer[(0, 21)].symbol(), "├");
        assert_eq!(buffer[(31, 21)].symbol(), "┴");
        assert_eq!(buffer[(79, 21)].symbol(), "┤");
        assert_eq!(
            buffer[(31, 20)].symbol(),
            "│",
            "the divider reaches the junction"
        );
    }

    /// The copy result takes over the bar for its whole visible window,
    /// then gives it back to the mode's own bar — the plan owner's
    /// two-second ruling on the brief's "for one draw", checked at the
    /// layer that actually decides what the bottom bar shows.
    #[test]
    fn bottom_bar_shows_the_copy_result_until_it_expires() {
        let mut state = AppState::new("build", false);
        let now = Instant::now();
        state.record_copy_result("copied (OSC 52)", now);

        assert_eq!(bottom_bar(&state, now), "copied (OSC 52) · q quit");
        assert_eq!(
            bottom_bar(&state, now + Duration::from_secs(1)),
            "copied (OSC 52) · q quit",
            "still visible partway through the window"
        );
        assert_eq!(
            bottom_bar(&state, now + Duration::from_secs(3)),
            "q quit · r rerun · c cancel · w watch · / search · ? help",
            "falls back to the mode's own bar once the result has expired"
        );
    }

    /// Copy's, Graph's, and Help's bar text is asserted equal to
    /// `help::COPY_KEYS`/`GRAPH_KEYS`/`HELP_KEYS` — the constants
    /// `help.rs`'s own overlay lists for these three modes (its own
    /// covering test,
    /// `keymap_lines_build_copy_graph_and_help_from_the_shared_constants`,
    /// checks the same constants from the overlay's side). A future edit
    /// that reintroduces a hand-copied literal in either `bottom_bar` or
    /// `help::keymap_lines` — rather than keeping both pointed at these
    /// constants — desyncs the two screens silently unless one of these
    /// two tests catches it.
    #[test]
    fn copy_graph_and_help_bars_match_their_shared_keymap_constants() {
        use crate::copy::CopyState;
        use crate::graph::GraphState;

        let now = Instant::now();
        let mut state = AppState::new("build", false);

        state.mode = Mode::Copy(CopyState::new_at(0));
        assert_eq!(bottom_bar(&state, now), help::COPY_KEYS);

        state.mode = Mode::Graph(GraphState::new(0));
        assert_eq!(bottom_bar(&state, now), help::GRAPH_KEYS);

        state.mode = Mode::Help;
        assert_eq!(bottom_bar(&state, now), help::HELP_KEYS);
    }
}
