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
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::state::{AppState, Mode};

mod graphpane;
mod header;
mod help;
mod logpane;
mod tree;

/// Below this floor, the tree and log panes have no room left to mean
/// anything, so the whole layout gives way to one message instead of
/// drawing a garbled screen (spec: "terminal too small", roughly 40x10).
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

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
            Paragraph::new("terminal too small (need at least 40x10)").alignment(Alignment::Center),
            area,
        );
        return;
    }

    // The outer frame carries the header and the bottom bar inside its
    // own border, exactly as the spec's mockup draws them
    // (`┌─ alba · run build ── ... ─┐` / `└─ q quit · ... ─┘`): a
    // leading `─ ` and trailing ` ` are baked into the title text itself
    // so the block's own border fill supplies the rest of the dashes.
    let outer = Block::bordered()
        .title_top(Line::from(format!("─ {} ", header::text(state, now))))
        .title_bottom(Line::from(format!("─ {} ", bottom_bar(state, now))));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    // Graph mode replaces the body entirely — no tree, no log pane, no
    // divider between them — rather than squeezing into either half:
    // the graph is the one thing on screen while it is active.
    if let Mode::Graph(graph) = &state.mode {
        graphpane::draw(frame, inner, state, graph);
        return;
    }

    // The tree pane's region includes its own right border, which is
    // the divider between it and the log pane — one column wider than
    // the tree's actual content width.
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(inner);
    let divider = Block::new().borders(Borders::RIGHT);
    let tree_area = divider.inner(panes[0]);
    frame.render_widget(divider, panes[0]);

    tree::draw(frame, tree_area, state, now);
    logpane::draw(frame, panes[1], state);

    // Help is an overlay, not a replacement: the tree and log panes stay
    // drawn underneath it (dimmed), unlike graph mode's own early return
    // above, which takes over the body entirely instead.
    if matches!(state.mode, Mode::Help) {
        help::draw(frame, inner);
    }
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
/// `n`/`N` live in the Normal-mode bar rather than Search's: they step
/// the *committed* search (`AppState::last_search`), a Normal-mode
/// binding (`input.rs`) the same way `j`/`k` are — advertising them
/// while still composing a query would claim a key that, at that point,
/// only ever types a character into it.
fn bottom_bar(state: &AppState, now: Instant) -> String {
    if let Some(result) = &state.last_copy_result
        && result.is_visible(now)
    {
        return format!("{} · q quit", result.message);
    }
    match &state.mode {
        Mode::Normal => {
            "q quit · r rerun · f force · c cancel · w watch · n next · N prev".to_string()
        }
        Mode::Search(search) => {
            let total = search.matches.len();
            let current = if total == 0 { 0 } else { search.current + 1 };
            format!(
                "/{} · {current}/{total} · Enter commit · Esc cancel",
                search.query
            )
        }
        Mode::Copy(_) => "hjkl/arrows move · v anchor · y copy · Esc cancel".to_string(),
        Mode::Graph(_) => "↑↓←→ move · Enter select · Esc back".to_string(),
        // While help is showing, only `Esc`/`?` (both close it) and
        // Ctrl-C do anything at all — `q` is swallowed as a plain
        // character the same as every other key `handle_modal_key`
        // doesn't recognize for this mode (see its own doc comment).
        Mode::Help => "Esc or ? close".to_string(),
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
/// the tree pane and its divider, and inside the title/footer rows
/// `logpane::draw` reserves — for a terminal of `width` × `height`
/// cells. `None` below the too-small floor, where `draw` paints nothing
/// but its one message and there is no pane to hit-test against.
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
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(inner);
    let rows = Layout::vertical([
        Constraint::Length(1), // "logs · {beam}" title
        Constraint::Min(0),    // output
        Constraint::Length(1), // follow state
    ])
    .split(panes[1]);
    Some(rows[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_pane_content_area_sits_past_the_tree_and_its_borders() {
        // Outer border: 1 cell each side. Tree pane + divider: 31
        // columns. Title row: 1 line. Footer row: 1 line.
        let area = log_pane_content_area(80, 24).expect("80x24 clears the floor");
        assert_eq!(area, Rect::new(32, 2, 47, 20));
    }

    #[test]
    fn log_pane_content_area_is_none_below_the_floor() {
        assert_eq!(log_pane_content_area(MIN_WIDTH - 1, 24), None);
        assert_eq!(log_pane_content_area(80, MIN_HEIGHT - 1), None);
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
            "q quit · r rerun · f force · c cancel · w watch · n next · N prev",
            "falls back to the mode's own bar once the result has expired"
        );
    }
}
