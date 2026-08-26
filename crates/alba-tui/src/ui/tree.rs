//! The left pane: one row per beam with a status glyph and a
//! right-aligned duration, the selected row reversed, and a footer that
//! tallies rows by status.
//!
//! The footer counts only the four statuses a beam can settle into
//! (`✔ ⚡ ✖ ○`) — the same four the spec's mockup's counts row shows —
//! and leaves a beam still `▶` running out of the tally: it has not
//! settled into an outcome yet, so counting it under any of the four
//! would misreport which bucket it will land in.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
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
            row_line(
                row,
                now,
                area.width as usize,
                index == state.selected,
                state.colour,
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[1]);

    frame.render_widget(Paragraph::new(counts_line(state)), rows[2]);
}

/// One tree row: the glyph in its status colour, the name and the
/// duration plain; the whole row reversed when selected.
fn row_line(
    row: &BeamRow,
    now: Instant,
    width: usize,
    selected: bool,
    colour: bool,
) -> Line<'static> {
    let glyph = glyph_for(&row.state);
    let duration = duration_text(&row.state, now);
    // The name column's char budget accounts for the glyph's *rendered*
    // width, not its char count: `⚡` paints two terminal cells, and
    // sizing every row as if every glyph painted one would push that
    // row's duration a column further right than the others.
    let name_width = width.saturating_sub(2 + glyph_width(glyph) + DURATION_WIDTH);
    let name = fit_name(&row.id, name_width);
    let mut spans = vec![
        Span::styled(
            format!(" {glyph}"),
            super::theme::status_style(&row.state, colour),
        ),
        Span::raw(format!(" {name:<name_width$}{duration:>DURATION_WIDTH$}")),
    ];
    if selected {
        for span in &mut spans {
            span.style = span.style.reversed();
        }
    }
    Line::from(spans)
}

/// The name column's text for now: truncation to fit `width` is a
/// later task's job.
fn fit_name(id: &str, _width: usize) -> String {
    id.to_string()
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

/// Shared with `ui/graphpane.rs`: the graph view's nodes use the exact
/// same status glyphs as the tree's rows (spec constraint), so this is
/// the one place that decides what each `BeamState` draws as.
pub(crate) fn glyph_for(state: &BeamState) -> &'static str {
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

/// The four buckets a beam can settle into, each `glyph count` pair
/// styled by `status_style` of a representative state for that bucket:
/// the same colour the tree's own rows would show that status in.
fn counts_line(state: &AppState) -> Line<'static> {
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
    let duration = Duration::from_secs(0);
    let buckets: [(&str, usize, BeamState); 4] = [
        (
            "✔",
            succeeded,
            BeamState::Done {
                status: BeamStatus::Succeeded,
                duration,
            },
        ),
        (
            "⚡",
            cached,
            BeamState::Done {
                status: BeamStatus::Cached,
                duration,
            },
        ),
        (
            "✖",
            failed,
            BeamState::Done {
                status: BeamStatus::Failed { exit_code: 1 },
                duration,
            },
        ),
        ("○", pending_or_cancelled, BeamState::Pending),
    ];
    let mut spans = Vec::with_capacity(buckets.len() * 2 - 1);
    for (index, (glyph, count, representative)) in buckets.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{glyph} {count}"),
            super::theme::status_style(representative, state.colour),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Style};

    #[test]
    fn a_failed_row_carries_the_status_colour_on_its_glyph() {
        use ratatui::style::Color;
        let row = BeamRow {
            id: "build".to_string(),
            state: BeamState::Done {
                status: BeamStatus::Failed { exit_code: 1 },
                duration: Duration::from_secs(1),
            },
        };
        let line = row_line(&row, Instant::now(), 30, false, true);
        assert_eq!(line.spans[0].style, Style::new().fg(Color::Red));
        assert_eq!(line.spans[0].content.trim(), "✖");
        let selected = row_line(&row, Instant::now(), 30, true, true);
        assert!(
            selected
                .spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        );
    }
}
