//! The left pane: one row per beam with a status glyph and a
//! right-aligned duration, the selected row reversed. The counts by
//! status that used to close this pane sit on the frame's footer now
//! (see `ui/footer.rs`).

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use alba_engine::BeamStatus;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
    // Pad by the name's *rendered* width, not its char count: a wide
    // character makes those two diverge, and padding by char count would
    // push the duration out of its column exactly the way an untruncated
    // wide name would.
    let pad = name_width.saturating_sub(name.width());
    let mut spans = vec![
        Span::styled(
            format!(" {glyph}"),
            super::theme::status_style(&row.state, colour),
        ),
        Span::raw(format!(" {name}{:pad$}{duration:>DURATION_WIDTH$}", "")),
    ];
    if selected {
        for span in &mut spans {
            span.style = span.style.reversed();
        }
    }
    Line::from(spans)
}

/// `id` cut to fit `width` rendered cells, with a trailing `…` when it
/// does not, so the duration column stays where it is. Beam ids may
/// contain any Unicode alphanumeric character (`alba-syntax`'s
/// `scan_ident` does not restrict them to ASCII), and a wide one paints
/// two cells, so this budgets by display width rather than char count.
fn fit_name(id: &str, width: usize) -> String {
    if id.width() <= width {
        return id.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1; // reserve one cell for the ellipsis
    let mut kept = String::new();
    let mut used = 0;
    for c in id.chars() {
        let cell = c.width().unwrap_or(0);
        if used + cell > budget {
            break;
        }
        kept.push(c);
        used += cell;
    }
    format!("{kept}…")
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Modifier, Style};
    use std::time::Duration;

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

    #[test]
    fn a_name_wider_than_its_column_is_truncated_with_an_ellipsis() {
        assert_eq!(
            fit_name("services:payment:integration", 19),
            "services:payment:i…"
        );
        assert_eq!(fit_name("build", 19), "build");
        assert_eq!(fit_name("abc", 0), "");
        assert_eq!(fit_name("abc", 1), "…");
    }

    #[test]
    fn a_name_exactly_as_wide_as_its_column_is_unchanged() {
        assert_eq!(fit_name("abcde", 5), "abcde");
    }

    #[test]
    fn a_name_one_cell_over_its_column_is_truncated() {
        assert_eq!(fit_name("abcdef", 5), "abcd…");
    }

    /// Beam ids may contain wide (double-cell) characters (`scan_ident`
    /// in `alba-syntax` is not ASCII-only), so `fit_name` budgets by
    /// display width: 5 wide chars (10 cells) truncated to 7 keeps only
    /// as many whole wide chars as fit alongside the ellipsis.
    #[test]
    fn a_wide_character_name_is_truncated_by_display_width_not_char_count() {
        assert_eq!(fit_name("国国国国国", 7), "国国国…");
    }

    /// A wide-character name never pushes the duration out of its
    /// column: the padding after a truncated name accounts for the
    /// name's rendered width, not its char count.
    #[test]
    fn a_wide_character_row_still_leaves_the_duration_in_its_column() {
        let row = BeamRow {
            id: "国".repeat(10),
            state: BeamState::Done {
                status: BeamStatus::Succeeded,
                duration: Duration::from_millis(1200),
            },
        };
        let line = row_line(&row, Instant::now(), 30, false, false);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            rendered.width(),
            30,
            "the row fills its column, no wider and no narrower"
        );
        assert!(
            rendered.ends_with(&format!("{:>DURATION_WIDTH$}", "1.2s")),
            "the duration keeps its place at the end: {rendered:?}"
        );
    }
}
