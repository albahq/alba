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
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::state::{AppState, Mode};

mod header;
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
        .title_bottom(Line::from(format!("─ {} ", bottom_bar(&state.mode))));
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

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
}

/// The always-available actions for the current mode. Only `Mode::Normal`
/// and `Mode::Search` have a keymap here: Copy, Graph, and Help belong to
/// the tasks that give those modes behaviour, so they fall back to the
/// one action that always applies rather than this task guessing at
/// their eventual keymaps.
///
/// `n`/`N` live in the Normal-mode bar rather than Search's: they step
/// the *committed* search (`AppState::last_search`), a Normal-mode
/// binding (`input.rs`) the same way `j`/`k` are — advertising them
/// while still composing a query would claim a key that, at that point,
/// only ever types a character into it.
fn bottom_bar(mode: &Mode) -> String {
    match mode {
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
        Mode::Copy | Mode::Graph | Mode::Help => "q quit".to_string(),
    }
}

/// How long something took, as one decimal of a second (`4.1s`) — the
/// same shape as `alba-cli`'s `format_duration`, kept as its own copy
/// here because the dependency direction (`alba-cli` → `alba-tui`) runs
/// the wrong way for this crate to import it.
fn format_duration(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}
