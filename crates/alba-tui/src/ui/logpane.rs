//! The right pane: the selected beam's output, or — once the project is
//! parked — the diagnostic that parked it, with the follow state
//! right-aligned on the title row. The pane has no last row of its own.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use alba_executors::Stream;

use crate::logs::{LogBuffer, LogLine, Row, Scroll};
use crate::state::{AppState, Mode, Phase};

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let rows = Layout::vertical([
        Constraint::Length(1), // "LOGS" / "DIAGNOSTIC", follow state at the right
        Constraint::Min(0),    // output
    ])
    .split(area);

    // A parked session has no beam worth showing: the diagnostic that
    // parked it, filed under a pseudo-beam key, is the only thing there
    // is to read (see `state::DIAGNOSTIC_LOG`). `displayed_log_key` is
    // the single source of truth for which key that is — copy mode's
    // `v`/`y` resolve their own buffer through the very same method, so
    // the selection highlight below and what `y` actually copies can
    // never disagree about what is on screen.
    let key = state.displayed_log_key();
    let buffer = state.logs.get(&key);

    // Two paragraphs on the one title row: the pane's name at the left
    // edge, the follow state at the right. Neither clears the row, so
    // the second does not paint over the first.
    frame.render_widget(Paragraph::new(title(state)), rows[0]);
    frame.render_widget(
        Paragraph::new(follow_text(buffer)).alignment(Alignment::Right),
        rows[0],
    );

    let body_height = rows[1].height as usize;
    // The query that should still be marked in the pane: the one being
    // typed while search is active, or the last completed one — kept
    // highlighted until a new search session replaces it (see
    // `AppState::last_search`).
    let query = match &state.mode {
        Mode::Search(search) => Some(search.query.as_str()),
        _ => state
            .last_search
            .as_ref()
            .map(|committed| committed.search.query.as_str()),
    };
    // `styled_line` needs a real buffer to look up a row's own line (for
    // the dimmed-stderr check and the full-line text search/copy need),
    // harmless to require, since a `None` buffer never produces a row to
    // begin with (`rows_shown` is empty).
    let lines: Vec<Line> = match buffer {
        Some(buffer) => buffer
            .view(body_height, rows[1].width as usize)
            .iter()
            .map(|row| styled_line(row, buffer, &state.mode, query, state.colour))
            .collect(),
        None => Vec::new(),
    };
    frame.render_widget(Paragraph::new(lines), rows[1]);
}

/// The pane's name. The selected beam is not repeated here: the tree's
/// reversed row already names it. Parked, the pane shows the diagnostic
/// that parked the project instead of any beam's output.
pub(crate) fn title(state: &AppState) -> &'static str {
    if matches!(state.phase, Phase::Parked) {
        "DIAGNOSTIC"
    } else {
        "LOGS"
    }
}

/// The follow state, right-aligned on the title row. No buffer yet
/// (nothing has run) reads the same as following: there is nothing to
/// be paused partway through.
pub(crate) fn follow_text(buffer: Option<&LogBuffer>) -> &'static str {
    let following = buffer.is_none_or(|buffer| matches!(buffer.scroll(), Scroll::Following));
    if following {
        "● following"
    } else {
        "↑ paused"
    }
}

/// The whole logical `LogLine` a row belongs to (`None` for the
/// truncation marker, which names no line). Search and the copy
/// highlight both need the full line's text rather than just this row's
/// own slice, so a match or a selection is found in the line's own
/// coordinates and then clipped to this row by `within`, the same line
/// looked up the same way whether it wrapped into one row or several;
/// the dimmed-stderr check reads the same line's `stream` off it.
fn full_line(buffer: &LogBuffer, line: Option<usize>) -> Option<&LogLine> {
    line.and_then(|line| buffer.line(line))
}

/// A `(from, to)` inclusive char range of the whole line, as the same
/// range inside `row`, or `None` when the two do not overlap.
fn within(row: &Row, from: usize, to: usize) -> Option<(usize, usize)> {
    let start = row.chars.start;
    let end = row.chars.end; // exclusive
    if end == 0 || to < start || from >= end {
        return None;
    }
    Some((from.max(start) - start, to.min(end - 1) - start))
}

/// The style a single rendered row gets: copy mode's selection highlight
/// when the row's own buffer-absolute line (`row.line`, `None` for the
/// truncation marker) falls inside the selection's covered span
/// (`covers_line`), the committed search's highlight otherwise. Copy
/// mode takes priority on whichever rows it covers: the two are not
/// meant to be shown blended. Both spans are computed over the whole
/// logical line (`full_line`) and clipped to this row by `within`, so a
/// match or a selection that crosses a wrap boundary still lights up
/// correctly on each of the rows it spans.
///
/// Starts from the row's own spans, carrying whatever colour ANSI
/// parsing already gave the line, and patches a reversed style over
/// the covered or matched range on top, so a search hit inside a
/// coloured `error` stays red as well as reversed.
///
/// Before either highlight, plain stderr (no colour of its own: an
/// ordinary `warning: ...`, never a line ANSI already coloured) is
/// dimmed, off when `colour` is off: the same visual cue the CLI's
/// headless renderers give stderr, so a beam's error output stands apart
/// from its stdout without a search or copy highlight to point at it.
///
/// Pulled out of `draw` so this composition can be tested directly over
/// the styled spans it produces. `draw` itself is only ever exercised
/// through `TestBackend::to_string()` in the snapshot tests, which drops
/// styles entirely and so cannot tell a covered, dimmed, or coloured row
/// from a plain one; an off-by-one in this wiring would pass every
/// existing snapshot silently.
fn styled_line(
    row: &Row,
    buffer: &LogBuffer,
    mode: &Mode,
    query: Option<&str>,
    colour: bool,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = row
        .spans
        .iter()
        .map(|(style, content)| Span::styled(content.clone(), *style))
        .collect();
    let line_ref = full_line(buffer, row.line);
    let full_text = line_ref.map(|line| line.text.as_str()).unwrap_or(&row.text);

    // Decided over the whole logical line, not just this row's wrapped
    // slice: a stderr line whose colour begins past the wrap boundary
    // must dim the same way on every row it wraps into, not just the
    // ones before its first coloured span. Falls back to the row's own
    // spans for the truncation marker, which has no line behind it.
    let own_colour = line_ref
        .map(|line| line.spans.as_slice())
        .unwrap_or(&row.spans)
        .iter()
        .any(|(style, _)| *style != Style::default());
    let dim_stderr =
        colour && line_ref.is_some_and(|line| line.stream == Stream::Stderr) && !own_colour;
    if dim_stderr {
        for span in &mut spans {
            span.style = span.style.dim();
        }
    }

    if let (Mode::Copy(selection), Some(line)) = (mode, row.line) {
        let covered = selection
            .covers_line(line, full_text.chars().count())
            .and_then(|(from, to)| within(row, from, to));
        if let Some((from, to)) = covered {
            return Line::from(patch(spans, from, to, Style::new().reversed()));
        }
    }
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        for (from, to) in match_ranges(full_text, query) {
            if let Some((from, to)) = within(row, from, to) {
                spans = patch(spans, from, to, Style::new().reversed());
            }
        }
    }
    Line::from(spans)
}

/// Every case-insensitive occurrence of `query` in `text`, as inclusive
/// char ranges. Folds case with `to_ascii_lowercase`, which never
/// changes a string's byte length, so byte positions found in the
/// folded copy convert to char indices of `text` itself.
fn match_ranges(text: &str, query: &str) -> Vec<(usize, usize)> {
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let mut pos = 0;
    while let Some(found) = haystack[pos..].find(&needle) {
        let start = pos + found;
        let end = start + needle.len();
        let from = text[..start].chars().count();
        let to = from + text[start..end].chars().count() - 1;
        ranges.push((from, to));
        pos = end;
    }
    ranges
}

/// `spans` with `style` patched over the inclusive char range
/// `[from, to]`, splitting whichever spans the range crosses. Styles
/// already on the spans stay underneath (`Style::patch`), so a search
/// highlight over a red `error` keeps it red and reversed.
fn patch(spans: Vec<Span<'static>>, from: usize, to: usize, style: Style) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(spans.len() + 2);
    let mut index = 0;
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let len = chars.len();
        let (start, end) = (index, index + len);
        index = end;
        if len == 0 || to < start || from >= end {
            out.push(span);
            continue;
        }
        let cut_from = from.max(start) - start;
        let cut_to = (to + 1).min(end) - start;
        if cut_from > 0 {
            out.push(Span::styled(
                chars[..cut_from].iter().collect::<String>(),
                span.style,
            ));
        }
        out.push(Span::styled(
            chars[cut_from..cut_to].iter().collect::<String>(),
            span.style.patch(style),
        ));
        if cut_to < len {
            out.push(Span::styled(
                chars[cut_to..].iter().collect::<String>(),
                span.style,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy::CopyState;
    use crate::logs::LogBuffer;
    use alba_engine::RunEvent;
    use alba_executors::Stream;

    #[test]
    fn the_title_names_the_pane_not_the_beam() {
        let mut state = AppState::new("build", false);
        assert_eq!(title(&state), "LOGS");
        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: unknown target `nope`\n".to_string(),
            },
            std::time::Instant::now(),
        );
        assert_eq!(title(&state), "DIAGNOSTIC");
    }

    #[test]
    fn the_follow_text_reads_following_without_a_buffer() {
        assert_eq!(follow_text(None), "● following");
    }

    /// The composition `draw` actually relies on: each row already
    /// names its own buffer-absolute line (`row.line`), and
    /// `covers_line` decides whether the selection covers it. An
    /// off-by-one in either step would not fail `TestBackend`'s
    /// snapshot at all (styles are dropped), so this asserts on the
    /// styled spans row by row instead.
    #[test]
    fn draw_marks_only_the_rows_the_selection_actually_covers() {
        let mut buffer = LogBuffer::new();
        for text in ["alpha", "bravo", "charlie", "delta"] {
            buffer.push(text, Stream::Stdout, false);
        }
        // A span from bravo's own start to charlie's third character,
        // inclusive — the same shape `CopyState::selected_text` uses.
        let mut copy = CopyState::new_at(1);
        copy.cursor = (2, 2);
        let mode = Mode::Copy(copy);
        let body_height = 4; // all four lines fit: row index == line index

        let rows: Vec<Line> = buffer
            .view(body_height, 80)
            .iter()
            .map(|row| styled_line(row, &buffer, &mode, None, false))
            .collect();

        assert_eq!(rows[0], Line::from("alpha"), "before the span: unstyled");
        assert_eq!(
            rows[1],
            Line::from(vec![Span::styled("bravo", Style::new().reversed())]),
            "the span's first row, covered whole"
        );
        assert_eq!(
            rows[2],
            Line::from(vec![
                Span::styled("cha", Style::new().reversed()),
                Span::raw("rlie"),
            ]),
            "the span's last row, up to its own end column"
        );
        assert_eq!(rows[3], Line::from("delta"), "after the span: unstyled");
    }

    /// Outside copy mode, the same composition falls through to the
    /// search highlight — checked here so `styled_line`'s two paths
    /// (copy vs. query) are both exercised through the one function
    /// `draw` actually calls.
    #[test]
    fn styled_line_falls_back_to_the_query_highlight_outside_copy_mode() {
        let row = Row {
            line: None,
            chars: 0..13,
            spans: vec![(Style::default(), "Compiling api".to_string())],
            text: "Compiling api".to_string(),
        };
        let buffer = LogBuffer::new();
        let line = styled_line(&row, &buffer, &Mode::Normal, Some("api"), false);
        assert_eq!(
            line,
            Line::from(vec![
                Span::raw("Compiling "),
                Span::styled("api", Style::new().reversed()),
            ])
        );
    }

    /// stderr without colour of its own is dimmed; stderr that brought
    /// colour keeps it.
    #[test]
    fn plain_stderr_is_dimmed_and_coloured_stderr_is_not() {
        use alba_executors::Stream;
        let mut buffer = LogBuffer::new();
        buffer.push("warning", Stream::Stderr, false);
        buffer.push("\u{1b}[31merror\u{1b}[0m", Stream::Stderr, false);
        buffer.push("out", Stream::Stdout, false);
        let rows = buffer.view(5, 80);
        let plain = styled_line(&rows[0], &buffer, &Mode::Normal, None, true);
        assert_eq!(plain.spans[0].style, Style::new().dim());
        let coloured = styled_line(&rows[1], &buffer, &Mode::Normal, None, true);
        assert_eq!(
            coloured.spans[0].style,
            Style::new().fg(ratatui::style::Color::Red)
        );
        let stdout = styled_line(&rows[2], &buffer, &Mode::Normal, None, true);
        assert_eq!(stdout.spans[0].style, Style::default());
        let no_colour = styled_line(&rows[0], &buffer, &Mode::Normal, None, false);
        assert_eq!(no_colour.spans[0].style, Style::default());
    }

    /// A wrapped stderr line whose colour starts only on its second row
    /// must dim (or not) the same way on both rows: the decision is
    /// about the whole logical line, not each wrapped slice of it. Width
    /// 4 wraps "aaaabb" into "aaaa" (default style) and "bb" (red),
    /// reproducing exactly the split the bug used to produce: the first
    /// row dimmed, the second row not, for what is really one coloured
    /// line.
    #[test]
    fn wrapped_stderr_line_is_dimmed_consistently_across_its_rows() {
        let mut buffer = LogBuffer::new();
        buffer.push("aaaa\u{1b}[31mbb\u{1b}[0m", Stream::Stderr, false);
        let rows = buffer.view(5, 4);
        assert_eq!(rows[0].text, "aaaa");
        assert_eq!(rows[1].text, "bb");

        let first = styled_line(&rows[0], &buffer, &Mode::Normal, None, true);
        let second = styled_line(&rows[1], &buffer, &Mode::Normal, None, true);
        assert_ne!(
            first.spans[0].style,
            Style::new().dim(),
            "line has colour, not dimmed"
        );
        assert_ne!(
            second.spans[0].style,
            Style::new().dim(),
            "same line, same treatment"
        );
    }

    /// `TestBackend::to_string()` drops styles, so the render snapshots
    /// cannot pin this: a unit test over `match_ranges` is what
    /// actually verifies every occurrence of a query gets found.
    #[test]
    fn match_ranges_finds_every_occurrence_case_insensitively() {
        assert_eq!(
            match_ranges("Compiling api, recompiling now", "compil"),
            vec![(0, 5), (17, 22)]
        );
    }

    #[test]
    fn match_ranges_is_empty_when_the_query_does_not_occur() {
        assert!(match_ranges("warning: unused import", "error").is_empty());
    }

    /// `TestBackend::to_string()` drops styles the same way it does for
    /// search: this unit test over `patch` is what actually pins the
    /// reversed style; the snapshot in `tests/render.rs` pins layout and
    /// the bottom bar instead.
    #[test]
    fn patch_marks_the_covered_chars() {
        let spans = vec![Span::raw("Compiling api")];
        let patched = patch(spans, 0, 3, Style::new().reversed());
        assert_eq!(
            patched,
            vec![
                Span::styled("Comp", Style::new().reversed()),
                Span::raw("iling api"),
            ]
        );
    }

    /// A range covering the whole span has nothing before or after to
    /// leave as a plain span.
    #[test]
    fn a_full_span_selection_has_a_single_styled_span() {
        let spans = vec![Span::raw("bravo")];
        let patched = patch(spans, 0, 4, Style::new().reversed());
        assert_eq!(
            patched,
            vec![Span::styled("bravo", Style::new().reversed())]
        );
    }

    /// Splitting must slice by character index, not byte offset: a
    /// multi-byte character split at the wrong boundary would panic.
    #[test]
    fn patch_does_not_panic_on_multibyte_characters_and_splits_by_char() {
        let spans = vec![Span::raw("héllo wörld")];
        let patched = patch(spans, 1, 3, Style::new().reversed());
        assert_eq!(
            patched,
            vec![
                Span::raw("h"),
                Span::styled("éll", Style::new().reversed()),
                Span::raw("o wörld"),
            ]
        );
    }

    /// A search highlight on a coloured span keeps the colour underneath.
    #[test]
    fn patch_keeps_the_underlying_colour() {
        use ratatui::style::Color;
        let spans = vec![Span::styled("error here", Style::new().fg(Color::Red))];
        let patched = patch(spans, 0, 4, Style::new().reversed());
        assert_eq!(patched.len(), 2);
        assert_eq!(patched[0].content, "error");
        assert_eq!(patched[0].style, Style::new().fg(Color::Red).reversed());
        assert_eq!(patched[1].content, " here");
        assert_eq!(patched[1].style, Style::new().fg(Color::Red));
    }
}
