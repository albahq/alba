//! The whole-frame layout: header, tree pane, log pane, and the bottom
//! bar — the interactive mirror of the CLI's headless renderers (see
//! `alba-cli/src/render/`), but a pure function of [`AppState`] rather
//! than a stream consumer.
//!
//! Nothing here samples a clock or touches the terminal: `now` arrives
//! as an argument, so a redraw is a deterministic function of its
//! inputs and a snapshot test owns the clock. See the spec's Layout
//! section for the mockup this module renders.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::widgets::Paragraph;

use crate::state::{AppState, Mode};

mod header;
mod logpane;
mod tree;

/// Below this floor, the tree and log panes have no room left to mean
/// anything, so the whole layout gives way to one message instead of
/// drawing a garbled screen (spec: "terminal too small", roughly 40x10).
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 10;

/// The tree pane's fixed width. The spec's mockup and prose both call
/// for 30 columns; an earlier sketch of this layout used 28, which this
/// implementation does not follow, so the mockup and the snapshots agree.
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

    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(1),    // body
        Constraint::Length(1), // bottom bar
    ])
    .split(area);

    header::draw(frame, rows[0], state, now);

    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH), Constraint::Min(1)]).split(rows[1]);
    tree::draw(frame, panes[0], state, now);
    logpane::draw(frame, panes[1], state);

    frame.render_widget(Paragraph::new(bottom_bar(&state.mode)), rows[2]);
}

/// The always-available actions for the current mode. Only `Mode::Normal`
/// has a keymap here: Search, Copy, Graph, and Help belong to the tasks
/// that give those modes behaviour, so every other mode falls back to the
/// one action that always applies rather than this task guessing at their
/// eventual keymaps.
fn bottom_bar(mode: &Mode) -> &'static str {
    match mode {
        Mode::Normal => "q quit · r rerun · f force · c cancel · w watch",
        Mode::Search | Mode::Copy | Mode::Graph | Mode::Help => "q quit",
    }
}

/// How long something took, as one decimal of a second (`4.1s`) — the
/// same shape as `alba-cli`'s `format_duration`, kept as its own copy
/// here because the dependency direction (`alba-cli` → `alba-tui`) runs
/// the wrong way for this crate to import it.
fn format_duration(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}
