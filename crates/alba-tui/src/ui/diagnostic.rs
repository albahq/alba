//! The diagnostic pane: what replaces the output pane while a watch
//! session is parked on a Beamfile that does not load and the failure
//! points at a span.
//!
//! Top to bottom: the title row (`DIAGNOSTIC / path:line:column`), the
//! `error:` line, a blank row, the source snippet with its line-number
//! gutter, a blank row and the `help:` line. A failure with no span
//! carries no report and keeps the plain-string log pane (`output.rs`).
//!
//! Snippet drawing is by display columns, never by chars: a tab is drawn
//! as [`TAB_WIDTH`] spaces, a wide character takes two cells, and the
//! caret row is placed through the same conversion so it sits under the
//! span whatever precedes it. The snippet never wraps; a line wider than
//! the pane is cut with `…`, and a span past the cut keeps its caret
//! clipped to the pane's last cell.

use alba_core::{DiagnosticReport, SnippetLine};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::beams::fit_name;
use super::theme::{ColourMode, Token, style};

/// A tab is drawn as this many spaces, in the snippet and so in the
/// caret row too.
const TAB_WIDTH: usize = 4;

const ERROR_LABEL: &str = "error: ";
const HELP_LABEL: &str = "help: ";

/// The pane for `report` occupying `area`: a title row, then the body
/// from the next row on, both one column in from the pane's edge (the
/// output pane's gutter) and short of its last column.
pub(super) fn draw(frame: &mut Frame, area: Rect, report: &DiagnosticReport, mode: ColourMode) {
    if area.height == 0 {
        return;
    }
    let x = area.x + 1;
    let width = area.width.saturating_sub(2);
    frame.render_widget(
        Paragraph::new(title_line(report, width as usize, mode)),
        Rect::new(x, area.y, area.width.saturating_sub(1), 1),
    );
    let body = Rect::new(x, area.y + 1, width, area.height - 1);
    frame.render_widget(
        Paragraph::new(body_lines(report, width as usize, mode)),
        body,
    );
}

/// `DIAGNOSTIC / path:line:column`, the location muted and cut with `…`
/// when the row is narrow.
fn title_line(report: &DiagnosticReport, width: usize, mode: ColourMode) -> Line<'static> {
    let head = "DIAGNOSTIC";
    let mut spans = vec![Span::styled(head, style(Token::Text, mode).bold())];
    let location = format!("{}:{}:{}", report.path, report.line, report.column);
    let room = width.saturating_sub(head.width() + " / ".width());
    if room > 0 {
        spans.push(Span::styled(" / ", style(Token::Line, mode)));
        spans.push(Span::styled(
            fit_name(&location, room),
            style(Token::TextMuted, mode),
        ));
    }
    Line::from(spans)
}

fn body_lines(report: &DiagnosticReport, width: usize, mode: ColourMode) -> Vec<Line<'static>> {
    let mut lines = labelled(
        ERROR_LABEL,
        style(Token::Error, mode).bold(),
        &report.message,
        style(Token::Text, mode).bold(),
        width,
    );
    lines.push(Line::default());
    lines.extend(snippet_lines(report, width, mode));
    if let Some(help) = &report.help {
        lines.push(Line::default());
        lines.extend(labelled(
            HELP_LABEL,
            style(Token::Success, mode).bold(),
            help,
            style(Token::Text, mode),
            width,
        ));
    }
    lines
}

/// `label` then `text`, hard-wrapped by display width; continuation rows
/// are indented under the text.
fn labelled(
    label: &'static str,
    label_style: Style,
    text: &str,
    text_style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let room = width.saturating_sub(label.width()).max(1);
    let mut rows = wrap(text, room).into_iter();
    let first = rows.next().unwrap_or_default();
    let mut lines = vec![Line::from(vec![
        Span::styled(label, label_style),
        Span::styled(first, text_style),
    ])];
    for row in rows {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(label.width())),
            Span::styled(row, text_style),
        ]));
    }
    lines
}

/// `text` cut into rows of at most `room` cells, at newlines and, past
/// that, wherever the row is full. A row is never empty unless its source
/// line is.
fn wrap(text: &str, room: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for source in text.lines() {
        let mut row = String::new();
        let mut used = 0;
        for c in source.chars() {
            let cell = c.width().unwrap_or(0);
            if used + cell > room && used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(c);
            used += cell;
        }
        rows.push(row);
    }
    rows
}

/// One source character as drawn: what it paints, where (display column
/// from the line's start) and how wide.
struct Glyph {
    text: String,
    column: usize,
    width: usize,
}

/// The glyphs of `text`, one per char, so a char index (the caret's unit)
/// indexes the result directly. A tab becomes [`TAB_WIDTH`] spaces and any
/// other control character one space, so nothing in the snippet can move
/// the terminal's cursor.
fn glyphs(text: &str) -> Vec<Glyph> {
    let mut column = 0;
    text.chars()
        .map(|c| {
            let (text, width) = match c {
                '\t' => (" ".repeat(TAB_WIDTH), TAB_WIDTH),
                c if c.is_control() => (" ".to_string(), 1),
                c => (c.to_string(), c.width().unwrap_or(0)),
            };
            let glyph = Glyph {
                text,
                column,
                width,
            };
            column += width;
            glyph
        })
        .collect()
}

/// The display column of char index `index`: where that char starts, or
/// the line's end for an index past the last char.
fn column_of(glyphs: &[Glyph], index: usize) -> usize {
    glyphs.get(index).map_or_else(
        || glyphs.last().map_or(0, |g| g.column + g.width),
        |g| g.column,
    )
}

fn snippet_lines(report: &DiagnosticReport, width: usize, mode: ColourMode) -> Vec<Line<'static>> {
    let digits = report
        .snippet
        .iter()
        .map(|line| line.number.to_string().len())
        .max()
        .unwrap_or(1);
    let bar = " │ ";
    let room = width.saturating_sub(digits + bar.width());
    let mut lines = Vec::new();
    for line in &report.snippet {
        lines.push(source_line(line, report, digits, room, mode));
        if line.faulty {
            lines.push(caret_line(line, report, digits, room, mode));
        }
    }
    lines
}

fn gutter(
    number: Option<usize>,
    digits: usize,
    number_style: Style,
    mode: ColourMode,
) -> Vec<Span<'static>> {
    let text = number.map_or_else(|| " ".repeat(digits), |n| format!("{n:>digits$}"));
    vec![
        Span::styled(text, number_style),
        Span::styled(" │ ", style(Token::Line, mode)),
    ]
}

fn source_line(
    line: &SnippetLine,
    report: &DiagnosticReport,
    digits: usize,
    room: usize,
    mode: ColourMode,
) -> Line<'static> {
    let (number_style, text_style) = if line.faulty {
        (style(Token::Text, mode).bold(), style(Token::Text, mode))
    } else {
        (style(Token::TextMuted, mode), style(Token::TextMuted, mode))
    };
    let mut spans = gutter(Some(line.number), digits, number_style, mode);
    let glyphs = glyphs(&line.text);
    let total = column_of(&glyphs, glyphs.len());
    // A line that does not fit gives its last cell to the ellipsis.
    let budget = if total > room {
        room.saturating_sub(1)
    } else {
        room
    };
    let faulty_style = style(Token::Error, mode).bold();
    let mut run = String::new();
    let mut run_style = text_style;
    for (index, glyph) in glyphs.iter().enumerate() {
        if glyph.column + glyph.width > budget {
            break;
        }
        let this = if line.faulty && report.caret.contains(&index) {
            faulty_style
        } else {
            text_style
        };
        if this != run_style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), run_style));
        }
        run_style = this;
        run.push_str(&glyph.text);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, run_style));
    }
    if total > room && room > 0 {
        spans.push(Span::styled("…", text_style));
    }
    Line::from(spans)
}

/// The caret row under the faulty line: the gutter blank with its bar
/// continued, then `^` across the span's display columns, clipped to the
/// room there is.
fn caret_line(
    line: &SnippetLine,
    report: &DiagnosticReport,
    digits: usize,
    room: usize,
    mode: ColourMode,
) -> Line<'static> {
    let mut spans = gutter(None, digits, Style::default(), mode);
    let glyphs = glyphs(&line.text);
    let start = column_of(&glyphs, report.caret.start);
    let end = column_of(&glyphs, report.caret.end);
    let start = start.min(room.saturating_sub(1));
    let end = end.min(room).max(start + 1);
    if room > 0 {
        spans.push(Span::raw(" ".repeat(start)));
        spans.push(Span::styled(
            "^".repeat(end - start),
            style(Token::Error, mode).bold(),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use alba_engine::RunEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Modifier;

    use super::super::draw as draw_frame;
    use super::*;
    use crate::state::{AppState, Mode};
    use crate::ui::theme::ColourMode;

    fn snippet(lines: &[(usize, &str, bool)]) -> Vec<SnippetLine> {
        lines
            .iter()
            .map(|(number, text, faulty)| SnippetLine {
                number: *number,
                text: (*text).to_string(),
                faulty: *faulty,
            })
            .collect()
    }

    fn report(lines: &[(usize, &str, bool)], caret: std::ops::Range<usize>) -> DiagnosticReport {
        DiagnosticReport {
            message: "unknown field `tset`".to_string(),
            path: "Beamfile".to_string(),
            line: 5,
            column: caret.start + 1,
            snippet: snippet(lines),
            caret,
            help: Some("did you mean `test`?".to_string()),
        }
    }

    fn parked(report: Option<DiagnosticReport>) -> AppState {
        let mut state = AppState::new("build", true);
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: unknown field `tset`\n".to_string(),
                report,
            },
            Instant::now(),
        );
        state
    }

    fn drawn(state: &AppState, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw_frame(frame, state, Instant::now()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    /// The terminal column where `needle` starts on row `y`, found cell
    /// by cell so a wide character before it counts for its two cells.
    fn column_of_text(buffer: &Buffer, y: u16, needle: &str) -> usize {
        let wanted: Vec<String> = needle.chars().map(|c| c.to_string()).collect();
        (0..buffer.area.width as usize)
            .find(|&x| {
                wanted
                    .iter()
                    .enumerate()
                    .all(|(i, symbol)| buffer[((x + i) as u16, y)].symbol() == symbol)
            })
            .expect("the needle is on the row")
    }

    fn row_with(buffer: &Buffer, needle: &str) -> u16 {
        rows(buffer)
            .iter()
            .position(|row| row.contains(needle))
            .unwrap_or_else(|| panic!("no row holds {needle:?}")) as u16
    }

    #[test]
    fn the_caret_sits_under_the_faulty_span() {
        let report = report(
            &[
                (4, "beam build {", false),
                (5, "    tset = \"cargo test\"", true),
                (6, "}", false),
            ],
            4..8,
        );
        let buffer = drawn(&parked(Some(report)), 100, 24);
        let faulty = row_with(&buffer, "tset =");
        assert_eq!(
            column_of_text(&buffer, faulty + 1, "^^^^"),
            column_of_text(&buffer, faulty, "tset"),
            "{:#?}",
            rows(&buffer)
        );
        let all = rows(&buffer);
        assert!(all[faulty as usize + 1].contains("│ "), "the bar continues");
        assert!(
            all.iter()
                .any(|row| row.contains("DIAGNOSTIC / Beamfile:5:5"))
        );
        assert!(
            all.iter()
                .any(|row| row.contains("error: unknown field `tset`"))
        );
        assert!(
            all.iter()
                .any(|row| row.contains("help: did you mean `test`?"))
        );
    }

    #[test]
    fn the_caret_follows_tabs_and_wide_characters() {
        // A tab (4 cells) and a wide character (2 cells) before the span.
        let report = report(&[(5, "\t国 tset", true)], 3..7);
        let buffer = drawn(&parked(Some(report)), 100, 24);
        let faulty = row_with(&buffer, "国");
        assert_eq!(
            column_of_text(&buffer, faulty + 1, "^^^^"),
            column_of_text(&buffer, faulty, "tset")
        );
    }

    #[test]
    fn the_faulty_span_is_bold_and_the_caret_is_in_error() {
        let report = report(&[(5, "tset = 1", true)], 0..4);
        let state = parked(Some(report)).with_colour(ColourMode::TrueColour);
        let buffer = drawn(&state, 100, 24);
        let faulty = row_with(&buffer, "tset = 1");
        let x = column_of_text(&buffer, faulty, "tset") as u16;
        let error = ratatui::style::Color::Rgb(0xF0, 0xA2, 0x9A);
        assert_eq!(buffer[(x, faulty)].fg, error);
        assert!(buffer[(x, faulty)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(x, faulty + 1)].symbol(), "^");
        assert_eq!(buffer[(x, faulty + 1)].fg, error);
        let rest = column_of_text(&buffer, faulty, "= 1") as u16;
        assert!(!buffer[(rest, faulty)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_long_line_is_cut_and_the_caret_stays_inside_the_pane() {
        let long = format!("{}tset", "x".repeat(200));
        let report = report(&[(5, &long, true)], 200..204);
        let buffer = drawn(&parked(Some(report)), 60, 24);
        let all = rows(&buffer);
        let faulty = row_with(&buffer, "xxxx");
        assert!(all[faulty as usize].trim_end().ends_with('…'), "{all:#?}");
        let caret = &all[faulty as usize + 1];
        assert!(caret.contains('^'), "{caret:?}");
        assert!(
            column_of_text(&buffer, faulty + 1, "^") < 60 - 2,
            "the caret stays in the pane"
        );
    }

    #[test]
    fn without_a_report_the_text_is_shown() {
        let state = parked(None);
        let buffer = drawn(&state, 100, 24);
        let all = rows(&buffer);
        assert!(all.iter().any(|row| row.contains("DIAGNOSTIC")));
        assert!(
            all.iter()
                .any(|row| row.contains("error: unknown field `tset`"))
        );
        assert!(state.diagnostic.is_none());
    }

    #[test]
    fn a_new_run_clears_the_report_and_a_new_break_replaces_it() {
        let mut state = parked(Some(report(&[(5, "tset", true)], 0..4)));
        assert!(state.diagnostic.is_some());
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: other\n".to_string(),
                report: None,
            },
            Instant::now(),
        );
        assert!(state.diagnostic.is_none(), "replaced, not kept");
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: again\n".to_string(),
                report: Some(report(&[(5, "tset", true)], 0..4)),
            },
            Instant::now(),
        );
        state.apply(
            &RunEvent::RunStarted {
                targets: Vec::new(),
                affected_by: None,
                beams: Vec::new(),
                edges: Vec::new(),
                details: Vec::new(),
            },
            Instant::now(),
        );
        assert!(state.diagnostic.is_none());
    }

    #[test]
    fn search_and_copy_are_not_offered_on_a_structured_diagnostic() {
        let mut state = parked(Some(report(&[(5, "tset", true)], 0..4)));
        state.enter_search();
        assert!(matches!(state.mode, Mode::Normal));
        state.enter_copy();
        assert!(matches!(state.mode, Mode::Normal));
    }

    #[test]
    fn a_break_while_searching_leaves_search_mode() {
        let mut state = AppState::new("build", true);
        state.enter_search();
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: x\n".to_string(),
                report: Some(report(&[(5, "tset", true)], 0..4)),
            },
            Instant::now(),
        );
        assert!(matches!(state.mode, Mode::Normal));
        assert!(state.last_search.is_none());
    }

    #[test]
    fn drawing_never_panics_at_any_size() {
        let report = report(
            &[
                (98, "a\tb国c", false),
                (99, "\t国国tset = \"a very long line indeed\"", true),
                (100, "", false),
            ],
            2..6,
        );
        let state = parked(Some(report));
        for height in 12..30 {
            for width in 40..120 {
                drawn(&state, width, height);
            }
        }
    }
}
