//! The left pane: one row per beam with a status glyph and a
//! right-aligned duration, the selected row reversed, and a footer that
//! tallies rows by status.
//!
//! The footer counts only the four statuses a beam can settle into
//! (`✔ ⚡ ✖ ○`) — the same four the spec's mockup's counts row shows —
//! and leaves a beam still `▶` running out of the tally: it has not
//! settled into an outcome yet, so counting it under any of the four
//! would misreport which bucket it will land in.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use alba_engine::BeamStatus;

use crate::state::{AppState, BeamRow, BeamState};

use super::format_duration;

/// Reserved for the duration column, right-aligned. Wide enough for the
/// durations Alba actually produces without the beam name stealing space
/// for the common (short) case.
const DURATION_WIDTH: usize = 8;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, now: Instant) {
    let rows = Layout::vertical([
        Constraint::Length(1), // "BEAMS" title
        Constraint::Min(0),    // one row per beam
        Constraint::Length(1), // per-status counts
    ])
    .split(area);

    frame.render_widget(Paragraph::new("BEAMS"), rows[0]);

    let lines: Vec<Line> = state
        .beams
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let text = row_text(row, now, area.width as usize);
            if index == state.selected {
                Line::styled(text, Style::default().reversed())
            } else {
                Line::from(text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[1]);

    frame.render_widget(Paragraph::new(counts_line(state)), rows[2]);
}

fn row_text(row: &BeamRow, now: Instant, width: usize) -> String {
    let glyph = glyph_for(&row.state);
    let duration = duration_text(&row.state, now);
    let dur_width = DURATION_WIDTH;
    // The name column's char budget accounts for the glyph's *rendered*
    // width, not its char count: `⚡` paints two terminal cells, and
    // sizing every row as if every glyph painted one would push that
    // row's duration a column further right than the others.
    let name_width = width.saturating_sub(2 + glyph_width(glyph) + dur_width);
    let name = &row.id;
    format!(" {glyph} {name:<name_width$}{duration:>dur_width$}")
}

/// The glyph's width in terminal cells. Hardcoded rather than pulled
/// from a unicode-width lookup: the status glyph set is fixed at five
/// characters (spec constraint), so a small table is simpler than a
/// dependency, and `⚡` (U+26A1) is the only one of the five that a
/// typical terminal renders double-width.
fn glyph_width(glyph: &str) -> usize {
    match glyph {
        "⚡" => 2,
        _ => 1,
    }
}

fn glyph_for(state: &BeamState) -> &'static str {
    match state {
        BeamState::Pending => "○",
        BeamState::Running { .. } => "▶",
        BeamState::Done { status, .. } => status_glyph(status),
    }
}

fn status_glyph(status: &BeamStatus) -> &'static str {
    match status {
        BeamStatus::Succeeded => "✔",
        BeamStatus::Cached => "⚡",
        BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => "✖",
        BeamStatus::Cancelled => "○",
    }
}

/// A running beam's duration ticks live (`now - since`) and carries a
/// trailing ellipsis, marking it as not yet final — unlike a finished
/// beam's duration, which is the one number it will ever report.
fn duration_text(state: &BeamState, now: Instant) -> String {
    match state {
        BeamState::Pending => String::new(),
        BeamState::Running { since } => {
            format!(
                "{}…",
                format_duration(now.saturating_duration_since(*since))
            )
        }
        BeamState::Done { duration, .. } => format_duration(*duration),
    }
}

fn counts_line(state: &AppState) -> String {
    let mut succeeded = 0;
    let mut cached = 0;
    let mut failed = 0;
    let mut pending_or_cancelled = 0;
    for row in &state.beams {
        match &row.state {
            BeamState::Pending => pending_or_cancelled += 1,
            BeamState::Running { .. } => {}
            BeamState::Done { status, .. } => match status {
                BeamStatus::Succeeded => succeeded += 1,
                BeamStatus::Cached => cached += 1,
                BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => failed += 1,
                BeamStatus::Cancelled => pending_or_cancelled += 1,
            },
        }
    }
    format!("✔ {succeeded}  ⚡ {cached}  ✖ {failed}  ○ {pending_or_cancelled}")
}
