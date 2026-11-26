//! The diagnostic pane: what replaces the output pane while a watch
//! session is parked on a Beamfile that does not load and the failure
//! points at a span.
//!
//! Top to bottom: the title row (`DIAGNOSTIC / path:line:column`), a
//! blank row (on a pane tall enough for the output's definition row,
//! and the first row to go when the pane is short), the `error:` line,
//! a blank row, the source snippet with its line-number
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
        Paragraph::new(body_lines(
            report,
            width as usize,
            body.height as usize,
            super::output::has_definition_row(area.height),
            mode,
        )),
        body,
    );
}

/// `DIAGNOSTIC / path:line:column`, the location muted and cut with `…`
/// when the row is narrow (see [`fit_location`]).
fn title_line(report: &DiagnosticReport, width: usize, mode: ColourMode) -> Line<'static> {
    let head = "DIAGNOSTIC";
    let mut spans = vec![Span::styled(head, style(Token::Text, mode).bold())];
    let room = width.saturating_sub(head.width() + " / ".width());
    if room > 0 {
        spans.push(Span::styled(" / ", style(Token::Line, mode)));
        spans.push(Span::styled(
            fit_location(report, room),
            style(Token::TextMuted, mode),
        ));
    }
    Line::from(spans)
}

/// `path:line:column` in at most `room` cells. `:line:column` says where
/// to look, so a location too long keeps it whole and loses the start of
/// its path instead, behind `…`, which keeps the file name. Only a room
/// too narrow for `…` and `:line:column` together cuts the end.
fn fit_location(report: &DiagnosticReport, room: usize) -> String {
    let position = format!(":{}:{}", report.line, report.column);
    let location = format!("{}{position}", report.path);
    if location.width() <= room {
        return location;
    }
    let Some(budget) = room
        .checked_sub(position.width() + 1)
        .filter(|&budget| budget > 0)
    else {
        return fit_name(&location, room);
    };
    let mut kept = Vec::new();
    let mut used = 0;
    for c in report.path.chars().rev() {
        let cell = c.width().unwrap_or(0);
        if used + cell > budget {
            break;
        }
        kept.push(c);
        used += cell;
    }
    let tail: String = kept.into_iter().rev().collect();
    format!("…{tail}{position}")
}

/// The rows under the title. `spaced` asks for a blank row first, as
/// the beams pane leaves under its title, so `error:` is level with
/// the first beam.
fn body_lines(
    report: &DiagnosticReport,
    width: usize,
    height: usize,
    spaced: bool,
    mode: ColourMode,
) -> Vec<Line<'static>> {
    let mut error = labelled(
        ERROR_LABEL,
        style(Token::Error, mode).bold(),
        &report.message,
        style(Token::Text, mode).bold(),
        width,
    );
    let mut help = report.help.as_ref().map_or_else(Vec::new, |help| {
        labelled(
            HELP_LABEL,
            style(Token::Success, mode).bold(),
            help,
            style(Token::Text, mode),
            width,
        )
    });
    let Snippet {
        mut before,
        faulty,
        mut after,
    } = snippet_lines(report, width, mode);
    let mut top = spaced;
    let mut blanks = true;
    let total = |error: &[Line], before: &[Line], after: &[Line], help: &[Line], blanks: bool| {
        let spacing = if blanks {
            usize::from(!error.is_empty()) + usize::from(!help.is_empty())
        } else {
            0
        };
        error.len() + before.len() + faulty.len() + after.len() + help.len() + spacing
    };
    // Short panes lose, in order: the blank row under the title, the
    // context before the faulty line, the context after it, the blank
    // rows, the wrapped rows of the message and of the help, then the
    // message itself. The faulty line, its caret and the help are what
    // stay.
    while usize::from(top) + total(&error, &before, &after, &help, blanks) > height {
        if top {
            top = false;
        } else if !before.is_empty() {
            before.remove(0);
        } else if !after.is_empty() {
            after.pop();
        } else if blanks {
            blanks = false;
        } else if error.len() > 1 {
            error.truncate(1);
        } else if help.len() > 1 {
            help.truncate(1);
        } else if !error.is_empty() {
            error.clear();
        } else {
            break;
        }
    }
    let mut lines = Vec::new();
    if top {
        lines.push(Line::default());
    }
    let has_error = !error.is_empty();
    lines.extend(error);
    if blanks && has_error {
        lines.push(Line::default());
    }
    lines.extend(before);
    lines.extend(faulty);
    lines.extend(after);
    if blanks && !help.is_empty() {
        lines.push(Line::default());
    }
    lines.extend(help);
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

/// `text` cut into rows of at most `room` cells, at newlines, then at
/// spaces; a word wider than `room` is cut where the row is full.
fn wrap(text: &str, room: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for source in text.lines() {
        let mut row = String::new();
        let mut used = 0;
        for word in source.split(' ').filter(|word| !word.is_empty()) {
            let word_width = word.width();
            if used > 0 && used + 1 + word_width <= room {
                row.push(' ');
                row.push_str(word);
                used += 1 + word_width;
                continue;
            }
            if used > 0 {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            for c in word.chars() {
                let cell = c.width().unwrap_or(0);
                if used + cell > room && used > 0 {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(c);
                used += cell;
            }
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

/// The snippet's rows, split around the faulty line (with its caret row)
/// so a short pane can drop the context on either side.
struct Snippet {
    before: Vec<Line<'static>>,
    faulty: Vec<Line<'static>>,
    after: Vec<Line<'static>>,
}

fn snippet_lines(report: &DiagnosticReport, width: usize, mode: ColourMode) -> Snippet {
    let digits = report
        .snippet
        .iter()
        .map(|line| line.number.to_string().len())
        .max()
        .unwrap_or(1);
    let bar = " │ ";
    let room = width.saturating_sub(digits + bar.width());
    let mut snippet = Snippet {
        before: Vec::new(),
        faulty: Vec::new(),
        after: Vec::new(),
    };
    for line in &report.snippet {
        if line.faulty {
            snippet
                .faulty
                .push(source_line(line, report, digits, room, mode));
            snippet
                .faulty
                .push(caret_line(line, report, digits, room, mode));
        } else {
            let row = source_line(line, report, digits, room, mode);
            if snippet.faulty.is_empty() {
                snippet.before.push(row);
            } else {
                snippet.after.push(row);
            }
        }
    }
    snippet
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
    use crate::state::AppState;
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

    fn typed(state: &mut AppState, text: &str) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        for c in text.chars() {
            state.handle_modal_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[test]
    fn search_and_copy_stay_offered_on_a_structured_diagnostic() {
        let state = parked(Some(report(&[(5, "tset", true)], 0..4)));
        let all = rows(&drawn(&state, 100, 24));
        assert!(all[22].contains("/ search"), "{:?}", all[22]);
        assert!(all[22].contains("v copy"), "{:?}", all[22]);
    }

    #[test]
    fn searching_shows_the_plain_text_and_escape_returns_to_the_structure() {
        let mut state = parked(Some(report(&[(5, "tset = 1", true)], 0..4)));
        state.enter_search();
        typed(&mut state, "tset");
        let buffer = drawn(&state, 100, 24);
        let all = rows(&buffer);
        assert!(!all.iter().any(|row| row.contains('^')), "{all:#?}");
        let plain = row_with(&buffer, "unknown field");
        let x = column_of_text(&buffer, plain, "tset") as u16;
        assert!(
            buffer[(x, plain)].modifier.contains(Modifier::REVERSED),
            "the match is highlighted"
        );
        state.leave_mode();
        let all = rows(&drawn(&state, 100, 24));
        assert!(all.iter().any(|row| row.contains('^')), "{all:#?}");
    }

    fn commit(state: &mut AppState, query: &str) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        state.enter_search();
        typed(state, query);
        state.handle_modal_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn escape_after_a_committed_search_returns_to_the_structure() {
        let mut state = parked(Some(report(&[(5, "tset = 1", true)], 0..4)));
        commit(&mut state, "tset");
        let all = rows(&drawn(&state, 100, 24));
        assert!(!all.iter().any(|row| row.contains('^')), "plain while kept");
        state.leave_mode();
        let all = rows(&drawn(&state, 100, 24));
        assert!(all.iter().any(|row| row.contains('^')), "{all:#?}");
    }

    #[test]
    fn a_committed_search_on_the_parked_diagnostic_is_matched_and_counted() {
        let mut state = parked(Some(report(&[(5, "tset = 1", true)], 0..4)));
        commit(&mut state, "tset");
        let buffer = drawn(&state, 100, 24);
        let all = rows(&buffer);
        assert!(all.iter().any(|row| row.contains("match 1/1")), "{all:#?}");
        let plain = row_with(&buffer, "unknown field");
        let x = column_of_text(&buffer, plain, "tset") as u16;
        assert!(buffer[(x, plain)].modifier.contains(Modifier::REVERSED));
    }

    fn broken(state: &mut AppState, text: &str) {
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: text.to_string(),
                report: Some(report(&[(5, "tset", true)], 0..4)),
            },
            Instant::now(),
        );
    }

    #[test]
    fn a_new_break_replaces_the_plain_text_and_resyncs_a_kept_search() {
        let mut state = parked(Some(report(&[(5, "tset", true)], 0..4)));
        commit(&mut state, "tset");
        broken(&mut state, "error: second tset\nsecond tset\n");
        let buffer = &state.logs[crate::state::DIAGNOSTIC_LOG];
        assert_eq!(buffer.len(), 2, "the first diagnostic is gone");
        let kept = state.last_search.as_ref().unwrap();
        assert_eq!(kept.search.matches.len(), 2, "matches follow the new text");
    }

    #[test]
    fn unparking_with_a_search_kept_on_the_diagnostic_moves_it_to_the_beam() {
        let mut state = parked(Some(report(&[(5, "tset", true)], 0..4)));
        commit(&mut state, "tset");
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![alba_core::BeamId("build".to_string())],
                affected_by: None,
                beams: vec![alba_core::BeamId("build".to_string())],
                edges: Vec::new(),
                details: Vec::new(),
            },
            Instant::now(),
        );
        let all = rows(&drawn(&state, 100, 24));
        assert!(
            !all.iter().any(|row| row.contains("match ")),
            "the counter is not shown on the beam: {all:#?}"
        );
        state.search_next();
        assert_eq!(state.last_search.as_ref().unwrap().beam, "build");
    }

    #[test]
    fn copying_shows_the_plain_text() {
        let mut state = parked(Some(report(&[(5, "tset = 1", true)], 0..4)));
        state.enter_copy();
        let all = rows(&drawn(&state, 100, 24));
        assert!(!all.iter().any(|row| row.contains('^')), "{all:#?}");
        assert!(all.iter().any(|row| row.contains("unknown field")));
    }

    /// A location too long for the title keeps its `:line:column`
    /// whole, the part that says where to look: the path is what gets
    /// cut, from its start, so the file name stays too.
    #[test]
    fn a_long_location_keeps_its_line_and_column() {
        let mut report = report(&[(5, "    tset = 1", true)], 4..8);
        report.path = "/home/someone/projects/a/deeply/nested/payment/Beamfile".to_string();
        let buffer = drawn(&parked(Some(report)), 80, 24);
        let title = rows(&buffer)
            .into_iter()
            .find(|row| row.contains("DIAGNOSTIC"))
            .expect("a title row");

        let title = title.trim_end();
        assert!(title.ends_with("/payment/Beamfile:5:5"), "{title:?}");
        assert!(title.contains("DIAGNOSTIC / …"), "{title:?}");
    }

    /// A blank row under the title, as the beams pane leaves under its
    /// own, so `error:` is level with the first beam.
    #[test]
    fn the_error_sits_a_blank_row_under_the_title() {
        let report = report(
            &[
                (4, "beam build {", false),
                (5, "    tset = 1", true),
                (6, "}", false),
            ],
            4..8,
        );
        let buffer = drawn(&parked(Some(report)), 80, 24);
        assert_eq!(row_with(&buffer, "DIAGNOSTIC"), 5);
        assert_eq!(row_with(&buffer, "error:"), 7);
    }

    /// When rows run short, that blank row is the first thing to go,
    /// before any context line.
    #[test]
    fn the_blank_row_under_the_title_goes_first() {
        let report = report(
            &[
                (3, "beam check {", false),
                (4, "  description \"x\"", false),
                (5, "  tset = 1", true),
                (6, "  run \"y\"", false),
                (7, "}", false),
            ],
            2..6,
        );
        // A pane of 11 rows: the title, then ten rows of body, one short
        // of the blank row under the title.
        let buffer = drawn(&parked(Some(report)), 80, 19);
        assert_eq!(row_with(&buffer, "error:"), 6);
        assert!(rows(&buffer).iter().any(|row| row.contains("3 │")));
    }

    /// Without a report the plain text starts on the same row as the
    /// structured `error:` would: a blank row under the title where the
    /// terminal is tall enough, none on a short one.
    #[test]
    fn without_a_report_the_text_starts_where_the_error_would() {
        let state = parked(None);
        assert_eq!(row_with(&drawn(&state, 80, 24), "error: unknown field"), 7);
        assert_eq!(row_with(&drawn(&state, 80, 15), "error: unknown field"), 6);
    }

    #[test]
    fn the_caret_and_help_survive_the_twelve_row_floor() {
        let report = report(
            &[
                (4, "beam build {", false),
                (5, "    tset = 1", true),
                (6, "}", false),
            ],
            4..8,
        );
        let all = rows(&drawn(&parked(Some(report)), 40, 12));
        assert!(all.iter().any(|row| row.contains('^')), "{all:#?}");
        assert!(all.iter().any(|row| row.contains("help:")), "{all:#?}");
        assert!(all.iter().any(|row| row.contains("5 │")), "{all:#?}");
    }

    #[test]
    fn long_messages_wrap_at_word_boundaries() {
        let mut broken = report(&[(5, "tset", true)], 0..4);
        broken.message = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda".into();
        let all = rows(&drawn(&parked(Some(broken.clone())), 60, 24));
        let panes: Vec<&str> = all
            .iter()
            .filter_map(|row| row.split('│').nth(1))
            .map(str::trim)
            .collect();
        let start = panes
            .iter()
            .position(|row| row.starts_with("error:"))
            .unwrap();
        let message: Vec<&str> = panes[start..]
            .iter()
            .take_while(|row| !row.is_empty())
            .copied()
            .collect();
        assert!(message.len() > 1, "the message wraps: {message:?}");
        let allowed: Vec<&str> = broken.message.split(' ').collect();
        for word in message.iter().flat_map(|row| row.split_whitespace()) {
            assert!(
                word == "error:" || allowed.contains(&word),
                "a word was cut: {word:?} in {message:?}"
            );
        }
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
