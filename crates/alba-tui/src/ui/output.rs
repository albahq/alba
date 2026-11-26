//! The right pane: the selected beam's output, or, once the project is
//! parked, the diagnostic that parked it.
//!
//! Top to bottom: the title row (`OUTPUT / beam`, the executor, the
//! pane's state at the right), the definition row (description and
//! location) and a blank row, both dropped on a terminal under 16 rows,
//! then the output. The pane's first column is the gutter, where a
//! stderr line gets its `▎`; titles and text start on the next column.
//! The last column is always kept for the scrollbar, drawn there only
//! when the output overflows, so a line wraps at the same width whether
//! or not the scrollbar shows.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::copy::{CopyState, Coverage};
use crate::logs::{Extent, LineKind, LogBuffer, LogLine, Row, Scroll, StreamFilter};
use crate::state::{AppState, BeamState, Mode, Phase};
use alba_executors::Stream;

use super::beams::{fit_name, scrollbar};
use super::diagnostic;
use super::theme::{ColourMode, Token, fill, remap_ansi, style};
use super::title;
use super::{ROWS_ABOVE_BODY, ROWS_BELOW_BODY};

/// Below this terminal height the definition row (and the blank row
/// under it) give their room to the output.
const DEFINITION_MIN_HEIGHT: u16 = 16;

/// The pane's body height at that terminal height.
const DEFINITION_MIN_ROWS: u16 = DEFINITION_MIN_HEIGHT - ROWS_ABOVE_BODY - ROWS_BELOW_BODY;

/// Whether a pane `height` rows high keeps the definition row. The
/// beams pane leaves a blank row under its title on the same condition,
/// so its list starts level with the output's blank separator row.
pub(super) fn has_definition_row(height: u16) -> bool {
    height >= DEFINITION_MIN_ROWS
}

/// Below this pane width the position badge would cover most of the
/// line it sits on, so it is not drawn.
const BADGE_MIN_WIDTH: u16 = 20;

/// Where each part of the pane goes. `draw` and
/// `ui::log_pane_content_area` both take it from [`layout`], so what
/// copy mode hit-tests and pages by is exactly what was drawn.
pub(super) struct PaneLayout {
    pub title: Rect,
    pub definition: Option<Rect>,
    pub gutter: Rect,
    pub text: Rect,
    pub rail: Rect,
}

/// The parts of an output pane occupying `pane`. The definition row is
/// kept only when the body has the rows of a terminal 16 rows high; the
/// blank row that separates it from the output goes with it, since it
/// has nothing left to separate. A `parked` pane shows the diagnostic,
/// which no beam defines: no definition row, only the blank row the
/// structured diagnostic leaves under its title, so both start `error:`
/// on the same row.
pub(super) fn layout(pane: Rect, parked: bool) -> PaneLayout {
    let row = |offset: u16| Rect::new(pane.x + 1, pane.y + offset, pane.width.saturating_sub(1), 1);
    let title = row(0);
    let (definition, above) = match (has_definition_row(pane.height), parked) {
        (true, false) => (Some(row(1)), 3),
        (true, true) => (None, 2),
        (false, _) => (None, 1),
    };
    let y = pane.y + above.min(pane.height);
    let height = pane.height.saturating_sub(above);
    PaneLayout {
        title,
        definition,
        gutter: Rect::new(pane.x, y, pane.width.min(1), height),
        text: Rect::new(pane.x + 1, y, pane.width.saturating_sub(2), height),
        rail: Rect::new(pane.right().saturating_sub(1), y, pane.width.min(1), height),
    }
}

/// What the title row says about the pane, at its right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    /// Following a beam still producing.
    Following,
    /// Scrolled away from the end.
    Paused,
    /// The beam finished; its output is read at the end.
    EndOfOutput,
    /// A search is being typed or was committed: match `current` of
    /// `total` (`0/0` when nothing matches).
    Match { current: usize, total: usize },
    /// Copy mode, over `lines` lines of the filtered view.
    Copy { lines: usize },
    /// The output is filtered to one stream.
    Filtered(StreamFilter),
}

impl PaneState {
    fn text(self) -> String {
        match self {
            Self::Following => "● following".to_string(),
            Self::Paused => "Ⅱ paused".to_string(),
            Self::EndOfOutput => "end of output".to_string(),
            Self::Match { current, total } => format!("match {current}/{total}"),
            Self::Copy { lines: 1 } => "COPY · 1 line".to_string(),
            Self::Copy { lines } => format!("COPY · {lines} lines"),
            Self::Filtered(StreamFilter::Stdout) => "▎ stdout only".to_string(),
            Self::Filtered(StreamFilter::Stderr) => "▎ stderr only".to_string(),
            Self::Filtered(StreamFilter::All) => String::new(),
        }
    }

    fn style(self, mode: ColourMode) -> Style {
        match self {
            Self::Following => style(Token::Success, mode),
            Self::EndOfOutput => style(Token::TextMuted, mode),
            Self::Paused | Self::Match { .. } | Self::Copy { .. } | Self::Filtered(_) => {
                style(Token::Accent, mode).bold()
            }
        }
    }
}

/// The pane's state. When several apply, the first of these wins:
/// copy mode, then a search (typed, or committed on this beam), then a
/// stream filter, then paused, then end of output, else following.
/// Copy and search are what the keys act on, so they come first. The
/// filter beats paused because nothing else on screen says the output
/// is filtered, while paused also shows as the position badge.
pub fn pane_state(state: &AppState) -> PaneState {
    let key = state.displayed_log_key();
    let buffer = state.logs.get(&key);
    let filter = state.effective_filter();
    if let Mode::Copy(copy) = &state.mode {
        let first = copy.anchor.0.min(copy.cursor.0);
        let last = copy.anchor.0.max(copy.cursor.0);
        let lines = buffer.map_or(0, |buffer| {
            (first..=last)
                .filter(|&index| buffer.line(index).is_some_and(|line| filter.admits(line)))
                .count()
        });
        return PaneState::Copy { lines };
    }
    let search = match &state.mode {
        Mode::Search(search) => Some(search),
        _ => state
            .last_search
            .as_ref()
            .filter(|committed| committed.beam == key)
            .map(|committed| &committed.search),
    };
    if let Some(search) = search {
        let total = search.matches.len();
        let current = if total == 0 { 0 } else { search.current + 1 };
        return PaneState::Match { current, total };
    }
    if filter != StreamFilter::All {
        return PaneState::Filtered(filter);
    }
    if buffer.is_some_and(|buffer| matches!(buffer.scroll(), Scroll::Paused { .. })) {
        return PaneState::Paused;
    }
    let finished = !matches!(state.phase, Phase::Parked)
        && state
            .selected_beam()
            .is_some_and(|beam| matches!(beam.state, BeamState::Done { .. }));
    if finished {
        PaneState::EndOfOutput
    } else {
        PaneState::Following
    }
}

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    // A parked session whose failure points at a span draws it as a
    // structured diagnostic; without one the plain text below stays.
    if let Some(report) = state
        .diagnostic
        .as_ref()
        .filter(|_| state.shows_structured_diagnostic())
    {
        diagnostic::draw(frame, area, report, state.colour);
        return;
    }
    let parts = layout(area, matches!(state.phase, Phase::Parked));
    let mode = state.colour;
    let pane_state = pane_state(state);
    frame.render_widget(
        Paragraph::new(title_line(state, pane_state, parts.title.width as usize)),
        parts.title,
    );
    if let Some(definition) = parts.definition {
        frame.render_widget(
            Paragraph::new(definition_line(state, definition.width as usize)),
            definition,
        );
    }

    // A parked session has no beam worth showing: the diagnostic that
    // parked it, filed under a pseudo-beam key, is the only thing there
    // is to read (see `state::DIAGNOSTIC_LOG`). `displayed_log_key` is
    // the single source of truth for which key that is: copy mode's
    // `v`/`y` resolve their own buffer through the very same method, so
    // the selection highlight below and what `y` actually copies can
    // never disagree about what is on screen.
    let Some(buffer) = state.logs.get(&state.displayed_log_key()) else {
        return;
    };
    let filter = state.effective_filter();
    let (height, width) = (parts.text.height as usize, parts.text.width as usize);
    // The search whose matches are marked: the one being typed, or the
    // last committed one while its own beam is on screen (see
    // `AppState::last_search`), the same rule `pane_state` counts by.
    let key = state.displayed_log_key();
    let search = match &state.mode {
        Mode::Search(search) => Some(search),
        _ => state
            .last_search
            .as_ref()
            .filter(|committed| committed.beam == key)
            .map(|committed| &committed.search),
    };
    let marks = search.map(|search| Marks {
        query: &search.query,
        current: search.current_line(),
    });
    let rows = buffer.view(height, width, filter);
    let gutter: Vec<Line> = rows
        .iter()
        .map(|row| gutter_cell(full_line(buffer, row.line), mode))
        .collect();
    let lines: Vec<Line> = rows
        .iter()
        .map(|row| {
            let line = full_line(buffer, row.line);
            let text = styled_line(row, buffer, &state.mode, marks, mode);
            decorated(row, line, text, width, mode)
        })
        .collect();
    // Copy mode's wholly copied lines are painted first, from the text
    // column to the pane's edge, so the text, the cursor cell and the
    // scrollbar drawn next keep the background.
    if let Mode::Copy(copy) = &state.mode {
        for (index, row) in rows.iter().enumerate() {
            if copy_coverage(copy, buffer, row) == Coverage::Whole {
                let y = parts.text.y + index as u16;
                let edge = Rect::new(parts.text.x, y, parts.text.width + parts.rail.width, 1);
                frame.buffer_mut().set_style(edge, selection_style(mode));
            }
        }
    }
    frame.render_widget(Paragraph::new(gutter), parts.gutter);
    frame.render_widget(Paragraph::new(lines), parts.text);

    let extent = buffer.extent(&rows, width, filter);
    if let Some(thumb) = scrollbar(extent.window.clone(), extent.rows, height) {
        let rail: Vec<Line> = (0..height)
            .map(|row| {
                if thumb.contains(&row) {
                    Line::styled("┃", style(Token::TextMuted, mode))
                } else {
                    Line::styled("│", style(Token::Line, mode))
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(rail), parts.rail);
    }

    if matches!(buffer.scroll(), Scroll::Paused { .. }) && area.width >= BADGE_MIN_WIDTH {
        draw_badge(frame, parts.text, &extent, mode);
    }
}

/// ` current/total ` at the bottom right of the output, over the end of
/// the last visible row: `current` is the last line in view, both
/// counted in lines of the filtered view, not rows.
fn draw_badge(frame: &mut Frame, text: Rect, extent: &Extent, mode: ColourMode) {
    if extent.current == 0 {
        return;
    }
    let badge = position_badge(extent.current, extent.lines);
    let badge_width = badge.width() as u16;
    if badge_width > text.width || text.height == 0 {
        return;
    }
    let badge_style = match mode {
        ColourMode::Monochrome => Style::new().reversed(),
        _ => fill(Token::Selection, mode).patch(style(Token::TextMuted, mode)),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(badge, badge_style)),
        Rect::new(
            text.right() - badge_width,
            text.bottom() - 1,
            badge_width,
            1,
        ),
    );
}

/// ` 1 240/2 400 `: the position, thousands separated by a space.
fn position_badge(current: usize, total: usize) -> String {
    format!(" {}/{} ", grouped(current), grouped(total))
}

fn grouped(number: usize) -> String {
    let digits = number.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(digit);
    }
    out
}

/// The title row: `OUTPUT / beam`, four spaces, `EXECUTOR kind detail`,
/// and the pane's state against the right edge. When the row is too
/// narrow the executor goes first, then the beam name is cut with `…`,
/// then dropped; the state is never cut. Parked, the pane is the
/// diagnostic's and names no beam. With nothing producing (no beam
/// before the first run or while parked, or a beam still pending)
/// `following` would not be true, so the state is left out.
fn title_line(state: &AppState, pane_state: PaneState, width: usize) -> Line<'static> {
    let mode = state.colour;
    let parked = matches!(state.phase, Phase::Parked);
    let head = if parked { "DIAGNOSTIC" } else { "OUTPUT" };
    let beam = state
        .selected_beam()
        .filter(|_| !parked)
        .map(|beam| beam.id.as_str());
    let pending = state
        .selected_beam()
        .is_some_and(|beam| matches!(beam.state, BeamState::Pending));
    let right = if (beam.is_none() || pending) && pane_state == PaneState::Following {
        String::new()
    } else {
        pane_state.text()
    };
    let room = width.saturating_sub(right.width() + 1);
    let executor = beam
        .and_then(|beam| state.details.get(beam))
        .map(|details| executor_spans(&details.executor, mode))
        .unwrap_or_default();
    let spans = title::breadcrumb(head, beam, executor, room, mode);
    title::with_right(spans, Span::styled(right, pane_state.style(mode)), width)
}

/// `    EXECUTOR kind detail`: the label muted bold, the kind in `text`,
/// what distinguishes it muted.
fn executor_spans(executor: &alba_engine::ExecutorLabel, mode: ColourMode) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::raw("    "),
        Span::styled("EXECUTOR", style(Token::TextMuted, mode).bold()),
        Span::styled(format!(" {}", executor.kind), style(Token::Text, mode)),
    ];
    if let Some(detail) = &executor.detail {
        spans.push(Span::styled(
            format!(" {detail}"),
            style(Token::TextMuted, mode),
        ));
    }
    spans
}

/// The definition row: the description at the left and the location at
/// the right, both muted. Either may be missing; nothing is drawn in
/// its place. When both do not fit, the description is cut first.
fn definition_line(state: &AppState, width: usize) -> Line<'static> {
    let muted = style(Token::TextMuted, state.colour);
    let details = state
        .selected_beam()
        .filter(|_| !matches!(state.phase, Phase::Parked))
        .and_then(|beam| state.details.get(&beam.id));
    let Some(details) = details else {
        return Line::default();
    };
    let location = details
        .location
        .as_deref()
        .map(|location| fit_name(location, width))
        .unwrap_or_default();
    let room = if location.is_empty() {
        width
    } else {
        width.saturating_sub(location.width() + 1)
    };
    let description = details
        .description
        .as_deref()
        .map(|description| fit_name(description, room))
        .unwrap_or_default();
    let pad = width.saturating_sub(description.width() + location.width());
    Line::from(vec![
        Span::styled(description, muted),
        Span::raw(" ".repeat(pad)),
        Span::styled(location, muted),
    ])
}

/// The gutter cell of a row: `▎` in `line` on every row of a stderr
/// line, blank otherwise.
fn gutter_cell(line: Option<&LogLine>, mode: ColourMode) -> Line<'static> {
    match line.map(|line| line.kind) {
        Some(LineKind::Output(Stream::Stderr)) => Line::styled("▎", style(Token::Line, mode)),
        _ => Line::default(),
    }
}

/// The style a line's own text is drawn in before any highlight: the
/// command's colours on the palette for output, bold for the command
/// echo, the notice or end line's tone for Alba's lines.
fn base_style(kind: LineKind, mode: ColourMode) -> Option<Style> {
    match kind {
        LineKind::Output(_) => None,
        LineKind::Command => Some(style(Token::Text, mode).bold()),
        LineKind::Notice(alba_engine::NoticeLevel::Info) => Some(style(Token::TextMuted, mode)),
        LineKind::Notice(alba_engine::NoticeLevel::Error) => Some(style(Token::Error, mode).bold()),
        LineKind::End(tone) => {
            let token = match tone {
                crate::logs::EndTone::Success => Token::Success,
                crate::logs::EndTone::Error => Token::Error,
                crate::logs::EndTone::Cached => Token::Cached,
                crate::logs::EndTone::Muted => Token::TextMuted,
            };
            Some(style(token, mode).bold())
        }
    }
}

/// A row's text with the lead of an Alba line before it: `── ` on the
/// first row of a notice or end line, blank on its other rows, so they
/// carry no dashes; the first row also gets ` ───…` after its text, up
/// to the pane's edge.
fn decorated(
    row: &Row,
    line: Option<&LogLine>,
    text: Line<'static>,
    width: usize,
    mode: ColourMode,
) -> Line<'static> {
    if row.lead == 0 {
        return text;
    }
    let opens = row.chars.start == 0 && !line.is_some_and(|line| line.continuation);
    let dashes = style(Token::Line, mode);
    let mut spans = Vec::with_capacity(text.spans.len() + 2);
    if opens {
        spans.push(Span::styled("── ", dashes));
    } else {
        spans.push(Span::raw(" ".repeat(row.lead)));
    }
    let used = row.lead + text.width();
    spans.extend(text.spans);
    let left = width.saturating_sub(used);
    if opens && left >= 2 {
        spans.push(Span::styled(format!(" {}", "─".repeat(left - 1)), dashes));
    }
    Line::from(spans)
}

/// The whole logical `LogLine` a row belongs to (`None` for the
/// truncation marker, which names no line). Search and the copy
/// highlight both need the full line's text rather than just this row's
/// own slice, so a match or a selection is found in the line's own
/// coordinates and then clipped to this row by `within`, the same line
/// looked up the same way whether it wrapped into one row or several;
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

/// A search to mark in the pane: its query, and the line of its
/// current match. `matches` holds lines, and `match i/n` counts them, so
/// every occurrence on the current line is drawn as the current match.
#[derive(Debug, Clone, Copy)]
struct Marks<'a> {
    query: &'a str,
    current: Option<usize>,
}

/// The background of what copy mode copies: `selection`, reversed in
/// monochrome.
fn selection_style(mode: ColourMode) -> Style {
    match mode {
        ColourMode::Monochrome => Style::new().reversed(),
        _ => fill(Token::Selection, mode),
    }
}

/// The cursor cell in copy mode: `on-accent` on `accent`; in
/// monochrome, the one cell of its reversed row not reversed, and bold.
fn copy_cursor_style(mode: ColourMode) -> Style {
    match mode {
        ColourMode::Monochrome => Style::new().not_reversed().bold(),
        _ => fill(Token::Accent, mode).patch(style(Token::OnAccent, mode)),
    }
}

/// The current search match: `on-accent` bold on `accent`; reversed
/// bold in monochrome.
fn current_match_style(mode: ColourMode) -> Style {
    match mode {
        ColourMode::Monochrome => Style::new().reversed().bold(),
        _ => fill(Token::Accent, mode)
            .patch(style(Token::OnAccent, mode))
            .bold(),
    }
}

/// Any other search match: the `selection` background under the text's
/// own colour; underlined in monochrome, where there is no background,
/// reversed already marks the current match and bold would not show on
/// lines already bold (the command echo, error notices, end lines).
fn other_match_style(mode: ColourMode) -> Style {
    match mode {
        ColourMode::Monochrome => Style::new().underlined(),
        _ => fill(Token::Selection, mode),
    }
}

/// A rendered row's text, styled. In copy mode what `y` copies is drawn
/// in `text` on `selection`: a whole line's row gets its background
/// from `draw`, up to the pane's edge; a partly copied line gets it on
/// the copied characters only. The cursor cell is on `accent`. Copy
/// mode takes priority over search marks on the rows it touches.
///
/// Starts from the row's own spans, carrying the colour ANSI parsing
/// gave the line, and patches the highlight over the covered range, so
/// a match inside a coloured `error` keeps its red on `selection`.
/// Matches and the cursor are found over the whole logical line
/// (`full_line`) and clipped to this row by `within`, so one that
/// crosses a wrap boundary lights up on each row it spans.
fn styled_line(
    row: &Row,
    buffer: &LogBuffer,
    mode: &Mode,
    marks: Option<Marks>,
    colour: ColourMode,
) -> Line<'static> {
    let line_ref = full_line(buffer, row.line);
    let base = line_ref.and_then(|line| base_style(line.kind, colour));
    let mut spans: Vec<Span<'static>> = row
        .spans
        .iter()
        .map(|(style, content)| {
            let style = base.unwrap_or_else(|| remap_ansi(*style, colour));
            Span::styled(content.clone(), style)
        })
        .collect();
    let echo_opens = line_ref
        .is_some_and(|line| line.kind == LineKind::Command && !line.continuation)
        && row.chars.start == 0;
    if echo_opens && !row.text.is_empty() {
        spans = patch(spans, 0, 0, style(Token::Accent, colour));
    }
    let full_text = line_ref.map(|line| line.text.as_str()).unwrap_or(&row.text);

    if let (Mode::Copy(copy), Some(line)) = (mode, row.line) {
        let text = style(Token::Text, colour);
        let coverage = copy_coverage(copy, buffer, row);
        match coverage {
            Coverage::Outside => {}
            // `draw` paints the rest of the row up to the pane's edge;
            // the selection also goes over the text, so a command's own
            // background does not hide it.
            Coverage::Whole => {
                let selected = selection_style(colour).patch(text);
                spans = spans
                    .into_iter()
                    .map(|span| {
                        let style = span.style.patch(selected);
                        span.style(style)
                    })
                    .collect();
            }
            Coverage::Chars(from, to) => {
                if let Some((from, to)) = within(row, from, to) {
                    spans = patch(spans, from, to, selection_style(colour).patch(text));
                }
            }
        }
        let has_cursor = line == copy.cursor.0;
        if has_cursor {
            spans = with_cursor(spans, row, copy.cursor.1, colour);
        }
        if has_cursor || coverage != Coverage::Outside {
            return Line::from(spans);
        }
    }
    if let Some(marks) = marks.filter(|marks| !marks.query.is_empty()) {
        let highlight = if row.line.is_some() && row.line == marks.current {
            current_match_style(colour)
        } else {
            other_match_style(colour)
        };
        for (from, to) in match_ranges(full_text, marks.query) {
            if let Some((from, to)) = within(row, from, to) {
                spans = patch(spans, from, to, highlight);
            }
        }
    }
    Line::from(spans)
}

/// What the copy selection takes of the line `row` belongs to; the
/// truncation marker belongs to none.
fn copy_coverage(copy: &CopyState, buffer: &LogBuffer, row: &Row) -> Coverage {
    match full_line(buffer, row.line).zip(row.line) {
        Some((line, index)) => copy.coverage(index, line.text.chars().count()),
        None => Coverage::Outside,
    }
}

/// `spans` with the copy cursor's cell at `column` of the line, when
/// that column is on this row. An empty line has no character to mark:
/// its one row gets a blank cursor cell instead.
fn with_cursor(
    spans: Vec<Span<'static>>,
    row: &Row,
    column: usize,
    colour: ColourMode,
) -> Vec<Span<'static>> {
    if row.chars.is_empty() && row.chars.start == 0 {
        let mut spans = spans;
        spans.push(Span::styled(" ", copy_cursor_style(colour)));
        return spans;
    }
    match within(row, column, column) {
        Some((from, to)) => patch(spans, from, to, copy_cursor_style(colour)),
        None => spans,
    }
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
    use crate::search::SearchState;
    use alba_core::BeamId;
    use alba_engine::{BeamDetails, BeamStatus, ExecutorLabel, NoticeLevel, RunEvent};
    use alba_executors::{OutputLine, Stream};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};
    use std::time::{Duration, Instant};

    // With one short beam at 80 columns the beams pane keeps its 20-cell
    // floor: the divider sits on column 22, the gutter on 23, the text
    // runs from 24 to 76 and the scrollbar takes column 77. At 24 rows
    // the title is row 5, the definition row 6, row 7 is blank and the
    // output fills rows 8 to 20.
    const GUTTER: u16 = 23;
    const TEXT: u16 = 24;
    const RAIL: u16 = 77;
    const TITLE: u16 = 5;
    const FIRST: u16 = 8;
    const LAST: u16 = 20;

    const ERROR: Color = Color::Rgb(0xF0, 0xA2, 0x9A);
    const LINE: Color = Color::Rgb(0x62, 0x6A, 0x67);
    const ACCENT: Color = Color::Rgb(0xF3, 0xC2, 0x80);
    const SUCCESS: Color = Color::Rgb(0xA5, 0xCF, 0x9A);
    const MUTED: Color = Color::Rgb(0xA1, 0xA7, 0xA4);
    const SELECTION: Color = Color::Rgb(0x35, 0x3C, 0x40);

    fn id(name: &str) -> BeamId {
        BeamId(name.to_string())
    }

    fn details(name: &str) -> BeamDetails {
        BeamDetails {
            id: id(name),
            description: Some("Run the test suite".to_string()),
            location: Some("Beamfile:23".to_string()),
            executor: ExecutorLabel {
                kind: "shell".to_string(),
                detail: Some("(embedded)".to_string()),
            },
            commands: Vec::new(),
        }
    }

    /// A run of the single beam `name`, started, in truecolour.
    fn started(name: &str) -> AppState {
        let mut state = AppState::new(name, false).with_colour(ColourMode::TrueColour);
        let now = Instant::now();
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id(name)],
                affected_by: None,
                beams: vec![id(name)],
                edges: Vec::new(),
                details: vec![details(name)],
            },
            now,
        );
        state.apply(&RunEvent::BeamStarted { id: id(name) }, now);
        state
    }

    fn write(state: &mut AppState, stream: Stream, text: &str) {
        state.apply(
            &RunEvent::BeamOutput {
                id: id("test"),
                line: OutputLine {
                    stream,
                    text: text.to_string(),
                },
                replayed: false,
            },
            Instant::now(),
        );
    }

    fn drawn(state: &AppState, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, state, Instant::now()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer, y: u16, columns: std::ops::Range<u16>) -> String {
        columns.map(|x| buffer[(x, y)].symbol()).collect()
    }

    fn title_row(state: &AppState) -> String {
        text(&drawn(state, 80, 24), TITLE, TEXT..78)
    }

    #[test]
    fn stderr_lines_carry_the_gutter_marker_and_full_colour() {
        let mut state = started("test");
        write(&mut state, Stream::Stderr, "\u{1b}[31merror\u{1b}[0m: boom");
        write(&mut state, Stream::Stdout, "plain");
        let buffer = drawn(&state, 80, 24);

        assert_eq!(buffer[(GUTTER, FIRST)].symbol(), "▎");
        assert_eq!(buffer[(GUTTER, FIRST)].fg, LINE);
        assert_eq!(text(&buffer, FIRST, TEXT..TEXT + 11), "error: boom");
        assert_eq!(
            buffer[(TEXT, FIRST)].fg,
            ERROR,
            "the command's red, remapped"
        );
        assert!(
            !buffer[(TEXT + 7, FIRST)].modifier.contains(Modifier::DIM),
            "plain stderr is not dimmed"
        );
        assert_eq!(
            buffer[(GUTTER, FIRST + 1)].symbol(),
            " ",
            "stdout has no marker"
        );
        assert_eq!(text(&buffer, FIRST + 1, TEXT..TEXT + 5), "plain");
    }

    #[test]
    fn alba_lines_span_the_pane_in_their_tone() {
        let mut state = started("test");
        let now = Instant::now();
        state.apply(
            &RunEvent::CommandStarted {
                id: id("test"),
                command: "cargo test".to_string(),
            },
            now,
        );
        state.apply(
            &RunEvent::BeamNotice {
                id: id("test"),
                level: NoticeLevel::Info,
                text: "cache bypassed\nsecond line".to_string(),
            },
            now,
        );
        state.apply(
            &RunEvent::BeamNotice {
                id: id("test"),
                level: NoticeLevel::Error,
                text: format!("template failed {}", "x".repeat(60)),
            },
            now,
        );
        state.apply(
            &RunEvent::BeamFinished {
                id: id("test"),
                status: BeamStatus::Succeeded,
                duration: Duration::from_millis(100),
            },
            now,
        );
        let buffer = drawn(&state, 80, 24);
        let row = |y: u16| text(&buffer, y, TEXT..RAIL);
        let filled = |head: &str| {
            let used = head.chars().count() + 4;
            format!("── {head} {}", "─".repeat(53 - used))
        };

        assert_eq!(row(FIRST).trim_end(), "$ cargo test");
        assert_eq!(buffer[(TEXT, FIRST)].fg, ACCENT);
        assert!(buffer[(TEXT, FIRST)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(TEXT + 2, FIRST)].modifier.contains(Modifier::BOLD));

        assert_eq!(row(FIRST + 1), filled("cache bypassed"));
        assert_eq!(buffer[(TEXT, FIRST + 1)].fg, LINE, "the dashes");
        assert_eq!(
            buffer[(TEXT + 3, FIRST + 1)].fg,
            MUTED,
            "an information notice"
        );
        assert_eq!(
            row(FIRST + 2).trim_end(),
            "   second line",
            "a notice's second line carries no dashes"
        );

        assert_eq!(
            row(FIRST + 3),
            format!("── template failed {}", "x".repeat(34))
        );
        assert_eq!(buffer[(TEXT + 3, FIRST + 3)].fg, ERROR);
        assert!(
            buffer[(TEXT + 3, FIRST + 3)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            row(FIRST + 4).trim_end(),
            format!("   {}", "x".repeat(26)),
            "a wrapped notice's next row carries no dashes"
        );

        assert_eq!(row(FIRST + 5), filled("✓ exit 0 · 0.1s"));
        assert_eq!(buffer[(TEXT + 3, FIRST + 5)].fg, SUCCESS);
        assert!(
            buffer[(TEXT + 3, FIRST + 5)]
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            buffer[(RAIL, FIRST + 5)].symbol(),
            " ",
            "the scrollbar column stays free"
        );
    }

    #[test]
    fn the_title_state_follows_the_pane() {
        let mut state = started("test");
        for index in 0..40 {
            write(&mut state, Stream::Stdout, &format!("line {index}"));
        }
        assert_eq!(pane_state(&state), PaneState::Following);
        assert!(title_row(&state).ends_with("● following"));

        state.logs.get_mut("test").unwrap().scroll_up(5);
        assert_eq!(pane_state(&state), PaneState::Paused);
        assert!(title_row(&state).ends_with("Ⅱ paused"));

        state.stream_filter = StreamFilter::Stderr;
        assert_eq!(
            pane_state(&state),
            PaneState::Filtered(StreamFilter::Stderr)
        );
        assert!(title_row(&state).ends_with("▎ stderr only"));
        state.stream_filter = StreamFilter::Stdout;
        assert!(title_row(&state).ends_with("▎ stdout only"));

        let mut search = SearchState::new();
        search.query = "line 1".to_string();
        search.update(&state.logs["test"], StreamFilter::Stdout);
        search.next();
        state.mode = Mode::Search(search);
        assert_eq!(
            pane_state(&state),
            PaneState::Match {
                current: 2,
                total: 11
            }
        );
        assert!(title_row(&state).ends_with("match 2/11"));

        let mut copy = CopyState::new_at(3);
        copy.cursor = (5, 0);
        state.mode = Mode::Copy(copy);
        assert_eq!(pane_state(&state), PaneState::Copy { lines: 3 });
        assert!(title_row(&state).ends_with("COPY · 3 lines"));
        state.mode = Mode::Copy(CopyState::new_at(4));
        assert_eq!(pane_state(&state), PaneState::Copy { lines: 1 });
        assert!(title_row(&state).ends_with("COPY · 1 line"));

        state.mode = Mode::Normal;
        state.stream_filter = StreamFilter::All;
        state.logs.get_mut("test").unwrap().follow_tail();
        state.apply(
            &RunEvent::BeamFinished {
                id: id("test"),
                status: BeamStatus::Succeeded,
                duration: Duration::from_millis(100),
            },
            Instant::now(),
        );
        assert_eq!(pane_state(&state), PaneState::EndOfOutput);
        assert!(title_row(&state).ends_with("end of output"));
    }

    const ON_ACCENT: Color = Color::Rgb(0x24, 0x22, 0x1E);
    const TEXT_FG: Color = Color::Rgb(0xE3, 0xE4, 0xDC);
    const SURFACE: Color = Color::Rgb(0x15, 0x17, 0x19);

    fn searching(state: &mut AppState, query: &str) {
        let mut search = SearchState::new();
        search.query = query.to_string();
        search.update(&state.logs["test"], StreamFilter::All);
        state.mode = Mode::Search(search);
    }

    /// The current match is `on-accent` bold on `accent`; every other
    /// match only gets the `selection` background, its colour kept.
    #[test]
    fn the_current_match_stands_out() {
        let mut state = started("test");
        write(&mut state, Stream::Stdout, "alpha err");
        write(&mut state, Stream::Stdout, "beta");
        write(&mut state, Stream::Stdout, "\u{1b}[31merr\u{1b}[0m gamma");
        searching(&mut state, "err");
        let buffer = drawn(&state, 80, 24);
        for x in TEXT + 6..TEXT + 9 {
            assert_eq!(buffer[(x, FIRST)].bg, ACCENT, "current, column {x}");
            assert_eq!(buffer[(x, FIRST)].fg, ON_ACCENT);
            assert!(buffer[(x, FIRST)].modifier.contains(Modifier::BOLD));
        }
        assert_eq!(buffer[(TEXT + 5, FIRST)].bg, SURFACE, "before the match");
        for x in TEXT..TEXT + 3 {
            assert_eq!(buffer[(x, FIRST + 2)].bg, SELECTION, "other, column {x}");
            assert_eq!(buffer[(x, FIRST + 2)].fg, ERROR, "its colour kept");
            assert!(!buffer[(x, FIRST + 2)].modifier.contains(Modifier::BOLD));
        }
        assert_eq!(buffer[(TEXT + 3, FIRST + 2)].bg, SURFACE, "after the match");

        if let Mode::Search(search) = &mut state.mode {
            search.next();
        }
        let buffer = drawn(&state, 80, 24);
        assert_eq!(buffer[(TEXT + 6, FIRST)].bg, SELECTION);
        assert_eq!(buffer[(TEXT, FIRST + 2)].bg, ACCENT);
        assert_eq!(buffer[(TEXT, FIRST + 2)].fg, ON_ACCENT);
    }

    /// Monochrome has no background: the current match is reversed
    /// bold, the others underlined, which still shows on a bold line.
    #[test]
    fn monochrome_matches_fall_back_to_attributes() {
        let mut state = started("test").with_colour(ColourMode::Monochrome);
        write(&mut state, Stream::Stdout, "err one");
        write(&mut state, Stream::Stdout, "err two");
        searching(&mut state, "err");
        let buffer = drawn(&state, 80, 24);
        let current = buffer[(TEXT, FIRST)].modifier;
        assert!(current.contains(Modifier::REVERSED | Modifier::BOLD));
        let other = buffer[(TEXT, FIRST + 1)].modifier;
        assert_eq!(other, Modifier::UNDERLINED);
        assert!(buffer[(TEXT + 4, FIRST)].modifier.is_empty());
    }

    /// A search committed on one beam marks that beam's output only.
    #[test]
    fn a_committed_search_marks_only_its_own_beam() {
        let mut state = AppState::new("test", false).with_colour(ColourMode::TrueColour);
        let now = Instant::now();
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id("test")],
                affected_by: None,
                beams: vec![id("test"), id("lint")],
                edges: Vec::new(),
                details: Vec::new(),
            },
            now,
        );
        for beam in ["test", "lint"] {
            state.apply(&RunEvent::BeamStarted { id: id(beam) }, now);
            state.apply(
                &RunEvent::BeamOutput {
                    id: id(beam),
                    line: OutputLine {
                        stream: Stream::Stdout,
                        text: "err here".to_string(),
                    },
                    replayed: false,
                },
                now,
            );
        }
        let mut search = SearchState::new();
        search.query = "err".to_string();
        search.update(&state.logs["test"], StreamFilter::All);
        state.last_search = Some(crate::state::CommittedSearch {
            beam: "test".to_string(),
            search,
        });
        let index = |state: &AppState, name: &str| {
            state
                .beams
                .iter()
                .position(|row| row.id.as_str() == name)
                .unwrap()
        };
        state.select(index(&state, "test"));
        assert_eq!(state.displayed_log_key(), "test");
        assert_eq!(drawn(&state, 80, 24)[(TEXT, FIRST)].bg, ACCENT);

        state.select(index(&state, "lint"));
        assert_eq!(state.displayed_log_key(), "lint");
        let buffer = drawn(&state, 80, 24);
        assert_eq!(text(&buffer, FIRST, TEXT..TEXT + 8), "err here");
        for x in TEXT..TEXT + 3 {
            assert_eq!(buffer[(x, FIRST)].bg, SURFACE, "column {x}");
            assert!(buffer[(x, FIRST)].modifier.is_empty(), "column {x}");
        }
    }

    /// Copy mode: the title counts the selected lines; those lines are
    /// on `selection` from the text column to the pane's edge, text in
    /// `text`, the cursor cell on `accent`; the stderr marker stays.
    #[test]
    fn the_copy_title_counts_selected_lines() {
        let mut state = started("test");
        write(&mut state, Stream::Stdout, "alpha");
        write(&mut state, Stream::Stderr, "\u{1b}[31mbravo\u{1b}[0m");
        write(&mut state, Stream::Stdout, "charlie");
        write(&mut state, Stream::Stdout, "");
        let mut copy = CopyState::new_at(1);
        copy.cursor = (2, 2);
        state.mode = Mode::Copy(copy);
        assert!(title_row(&state).ends_with("COPY · 2 lines"));
        let buffer = drawn(&state, 80, 24);
        // bravo is copied whole: its row runs to the pane's edge.
        for x in TEXT..=RAIL {
            assert_eq!(buffer[(x, FIRST + 1)].bg, SELECTION, "({x}, {})", FIRST + 1);
        }
        // charlie is copied up to the cursor: `cha` only.
        for x in TEXT..TEXT + 2 {
            assert_eq!(buffer[(x, FIRST + 2)].bg, SELECTION, "({x}, {})", FIRST + 2);
        }
        for y in [FIRST + 1, FIRST + 2] {
            assert_eq!(buffer[(TEXT, y)].fg, TEXT_FG, "row {y}");
        }
        assert_eq!(buffer[(GUTTER, FIRST + 1)].symbol(), "▎");
        assert_eq!(buffer[(GUTTER, FIRST + 1)].fg, LINE);
        assert_eq!(buffer[(GUTTER, FIRST + 1)].bg, SURFACE);
        assert_eq!(buffer[(TEXT + 2, FIRST + 2)].bg, ACCENT, "the cursor cell");
        assert_eq!(buffer[(TEXT + 2, FIRST + 2)].fg, ON_ACCENT);
        assert_eq!(buffer[(TEXT + 3, FIRST + 2)].bg, SURFACE, "past the cursor");
        assert_eq!(buffer[(RAIL, FIRST + 2)].bg, SURFACE);
        for y in [FIRST, FIRST + 3] {
            assert_eq!(buffer[(TEXT, y)].bg, SURFACE, "row {y}");
            assert_eq!(buffer[(RAIL, y)].bg, SURFACE, "row {y}");
        }

        // A cursor on an empty line still shows its cell.
        let mut copy = CopyState::new_at(3);
        copy.cursor = (3, 0);
        state.mode = Mode::Copy(copy);
        assert!(title_row(&state).ends_with("COPY · 1 line"));
        let buffer = drawn(&state, 80, 24);
        assert_eq!(buffer[(TEXT, FIRST + 3)].bg, ACCENT);
        assert_eq!(buffer[(TEXT + 1, FIRST + 3)].bg, SURFACE);
    }

    /// The cells drawn on `selection` or `accent` in the text column,
    /// row by row, without the blank fill a whole line gets up to the
    /// pane's edge: what the reader sees selected.
    fn highlighted(buffer: &Buffer, rows: std::ops::RangeInclusive<u16>) -> Vec<String> {
        rows.map(|y| {
            (TEXT..RAIL)
                .filter(|&x| [SELECTION, ACCENT].contains(&buffer[(x, y)].bg))
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
    }

    /// A selection from mid-line to mid-line: its first and last lines
    /// are drawn on `selection` only over the copied cells, the lines
    /// between to the pane's edge, and the highlighted cells read
    /// exactly what `y` copies.
    #[test]
    fn the_copy_highlight_is_what_gets_copied() {
        let mut state = started("test");
        for text in ["alpha", "bravo", "charlie", "delta"] {
            write(&mut state, Stream::Stdout, text);
        }
        let mut copy = CopyState::new_at(0);
        copy.anchor = (2, 3);
        copy.cursor = (0, 2);
        state.mode = Mode::Copy(copy);
        let buffer = drawn(&state, 80, 24);
        assert_eq!(buffer[(TEXT + 1, FIRST)].bg, SURFACE, "before the anchor");
        assert_eq!(buffer[(TEXT + 2, FIRST)].bg, ACCENT, "the cursor");
        assert_eq!(buffer[(TEXT + 5, FIRST)].bg, SURFACE, "past the first line");
        assert_eq!(buffer[(RAIL, FIRST + 1)].bg, SELECTION, "a whole line");
        assert_eq!(buffer[(TEXT + 3, FIRST + 2)].bg, SELECTION);
        assert_eq!(buffer[(TEXT + 4, FIRST + 2)].bg, SURFACE, "past the anchor");
        let copied = copy.selected_text(&state.logs["test"], StreamFilter::All);
        assert_eq!(copied, "pha\nbravo\nchar");
        let seen = highlighted(&buffer, FIRST..=FIRST + 3);
        assert_eq!(seen.join("\n").trim_end_matches('\n'), copied);
    }

    /// A command's own background does not hide the selection on a line
    /// copied whole, as it does not on a partly copied one.
    #[test]
    fn a_whole_copied_line_overrides_the_command_background() {
        let mut state = started("test");
        write(&mut state, Stream::Stdout, "\u{1b}[41mred\u{1b}[0m");
        write(&mut state, Stream::Stdout, "\u{1b}[41mred\u{1b}[0m");
        let mut copy = CopyState::new_at(0);
        copy.cursor = (1, 1);
        state.mode = Mode::Copy(copy);
        let buffer = drawn(&state, 80, 24);
        assert_eq!(buffer[(TEXT, FIRST)].bg, SELECTION, "the whole line");
        assert_eq!(buffer[(TEXT + 2, FIRST)].bg, SELECTION, "the whole line");
        assert_eq!(buffer[(TEXT, FIRST + 1)].bg, SELECTION, "the copied part");
    }

    /// A single line wrapped over two rows: the selection follows the
    /// wrap, covering only its own range on each row.
    #[test]
    fn a_wrapped_selection_covers_only_its_range() {
        let mut state = started("test");
        let long: String = ('a'..='z').cycle().take(120).collect();
        write(&mut state, Stream::Stdout, &long);
        let mut copy = CopyState::new_at(0);
        copy.anchor = (0, 50);
        copy.cursor = (0, 60);
        state.mode = Mode::Copy(copy);
        let buffer = drawn(&state, 80, 24);
        let copied = copy.selected_text(&state.logs["test"], StreamFilter::All);
        assert_eq!(copied, long[50..=60]);
        let seen = highlighted(&buffer, FIRST..=FIRST + 1);
        assert_eq!(seen, [long[50..53].to_string(), long[53..=60].to_string()]);
        assert_eq!(buffer[(RAIL, FIRST)].bg, SURFACE);
    }

    /// Before the first run there is no beam whose output is followed,
    /// so the title says nothing about following.
    #[test]
    fn a_pane_with_no_beam_claims_no_state() {
        let state = AppState::new("build", false);
        assert_eq!(title_row(&state).trim_end(), "OUTPUT");
    }

    /// A selected beam still waiting to start is not producing: the
    /// title says nothing about following it.
    #[test]
    fn a_pending_beam_claims_no_state() {
        let mut state = AppState::new("test", false).with_colour(ColourMode::TrueColour);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id("test")],
                affected_by: None,
                beams: vec![id("test")],
                edges: Vec::new(),
                details: Vec::new(),
            },
            Instant::now(),
        );
        assert_eq!(title_row(&state).trim_end(), "OUTPUT / test");
    }

    #[test]
    fn a_narrow_title_drops_the_executor_first() {
        let state = started("test");
        assert_eq!(
            title_row(&state).trim_end(),
            format!("OUTPUT / test    EXECUTOR shell (embedded) ● following")
        );
        assert_eq!(buffer_fg(&state, 80, TEXT + 9), ACCENT, "the beam name");

        let narrow = drawn(&state, 60, 24);
        let row = text(&narrow, TITLE, TEXT..58);
        assert!(row.starts_with("OUTPUT / test "), "{row:?}");
        assert!(!row.contains("EXECUTOR"), "{row:?}");
        assert!(row.ends_with("● following"), "{row:?}");

        let long = started("services:payment:integration");
        let narrow = drawn(&long, 60, 24);
        // The beams pane is capped at 24 cells: the text starts on 28.
        let row = text(&narrow, TITLE, 28..58);
        assert!(row.starts_with("OUTPUT / services… "), "{row:?}");
        assert!(row.ends_with("● following"), "{row:?}");
    }

    fn buffer_fg(state: &AppState, width: u16, x: u16) -> Color {
        drawn(state, width, 24)[(x, TITLE)].fg
    }

    #[test]
    fn the_badge_shows_the_position_only_when_paused() {
        let mut state = started("test");
        for index in 1..=2400 {
            write(&mut state, Stream::Stdout, &format!("line {index}"));
        }
        let badge = " 1 240/2 400 ";
        let width = badge.chars().count() as u16;

        let following = drawn(&state, 80, 24);
        assert!(!text(&following, LAST, TEXT..RAIL).contains('/'));

        state.logs.get_mut("test").unwrap().scroll_up(2400 - 1240);
        let paused = drawn(&state, 80, 24);
        assert_eq!(text(&paused, LAST, RAIL - width..RAIL), badge);
        assert_eq!(text(&paused, LAST, TEXT..TEXT + 9), "line 1240");
        assert_eq!(paused[(RAIL - 1, LAST)].bg, SELECTION);
        assert_eq!(paused[(RAIL - 1, LAST)].fg, MUTED);

        // At 40 columns the output pane is 15 cells wide: no badge.
        let narrow = drawn(&state, 40, 24);
        assert!(!text(&narrow, LAST, 0..40).contains("2 400"));
    }

    #[test]
    fn the_definition_row_hides_under_sixteen_rows() {
        let mut state = started("test");
        write(&mut state, Stream::Stdout, "first line");

        let tall = drawn(&state, 80, 16);
        assert_eq!(
            text(&tall, 6, TEXT..78),
            format!("Run the test suite{}Beamfile:23", " ".repeat(54 - 18 - 11))
        );
        assert_eq!(tall[(TEXT, 6)].fg, MUTED);
        assert_eq!(text(&tall, 8, TEXT..TEXT + 10), "first line");

        let short = drawn(&state, 80, 15);
        assert_eq!(text(&short, 6, TEXT..TEXT + 10), "first line");
        assert_eq!(
            super::super::log_pane_content_area(&state, 80, 15)
                .unwrap()
                .y,
            6
        );
        assert_eq!(
            super::super::log_pane_content_area(&state, 80, 16)
                .unwrap()
                .y,
            8
        );
    }

    #[test]
    fn a_wrapped_ansi_line_keeps_its_remapped_colour_on_every_row() {
        let mut state = started("test");
        write(
            &mut state,
            Stream::Stdout,
            &format!("\u{1b}[31m{}\u{1b}[0m", "e".repeat(120)),
        );
        let buffer = drawn(&state, 80, 24);
        for y in [FIRST, FIRST + 1, FIRST + 2] {
            assert_eq!(buffer[(TEXT, y)].symbol(), "e", "row {y}");
            assert_eq!(buffer[(TEXT, y)].fg, ERROR, "row {y}");
        }
    }

    #[test]
    fn a_wide_output_line_does_not_push_the_scrollbar() {
        let mut fits = started("test");
        write(&mut fits, Stream::Stdout, &"w".repeat(200));
        let short = drawn(&fits, 80, 24);
        assert_eq!(text(&short, FIRST, TEXT..RAIL), "w".repeat(53));
        assert_eq!(
            short[(RAIL, FIRST)].symbol(),
            " ",
            "no scrollbar when it fits"
        );

        let mut overflows = fits;
        for index in 0..40 {
            write(&mut overflows, Stream::Stdout, &format!("line {index}"));
        }
        write(&mut overflows, Stream::Stdout, &"w".repeat(200));
        let long = drawn(&overflows, 80, 24);
        // The last line wraps at the same 53 cells: 53, 53, 53, 41.
        assert_eq!(text(&long, LAST - 1, TEXT..RAIL), "w".repeat(53));
        assert_eq!(text(&long, LAST, TEXT..RAIL).trim_end(), "w".repeat(41));
        for y in FIRST..=LAST {
            let rail = long[(RAIL, y)].symbol();
            assert!(rail == "│" || rail == "┃", "row {y}: {rail:?}");
        }
        assert_eq!(
            long[(RAIL, LAST)].symbol(),
            "┃",
            "following: the thumb at the end"
        );
    }
    /// In monochrome a selected line is reversed by `draw`; its text
    /// keeps no colour of its own and the cursor cell is the one cell
    /// left unreversed, in bold. Lines outside the selection stay plain.
    #[test]
    fn a_monochrome_selection_marks_its_lines_and_the_cursor() {
        let mut state = started("test").with_colour(ColourMode::Monochrome);
        for text in ["alpha", "bravo", "charlie", "delta"] {
            write(&mut state, Stream::Stdout, text);
        }
        let mut copy = CopyState::new_at(1);
        copy.cursor = (2, 2);
        state.mode = Mode::Copy(copy);
        let buffer = drawn(&state, 80, 24);
        for y in [FIRST + 1, FIRST + 2] {
            assert!(buffer[(TEXT, y)].modifier.contains(Modifier::REVERSED));
        }
        assert!(
            buffer[(RAIL, FIRST + 1)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer[(TEXT + 3, FIRST + 2)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        let cursor = buffer[(TEXT + 2, FIRST + 2)].modifier;
        assert!(cursor.contains(Modifier::BOLD) && !cursor.contains(Modifier::REVERSED));
        for y in [FIRST, FIRST + 3] {
            assert!(buffer[(TEXT, y)].modifier.is_empty(), "row {y}");
        }
    }

    /// Outside copy mode, `styled_line` marks a search's matches; a row
    /// of no line (the truncation marker) is never the current match.
    #[test]
    fn styled_line_falls_back_to_the_query_highlight_outside_copy_mode() {
        let row = Row {
            lead: 0,
            line: None,
            chars: 0..13,
            spans: vec![(Style::default(), "Compiling api".to_string())],
            text: "Compiling api".to_string(),
        };
        let buffer = LogBuffer::new();
        let marks = Marks {
            query: "api",
            current: None,
        };
        let line = styled_line(
            &row,
            &buffer,
            &Mode::Normal,
            Some(marks),
            ColourMode::Monochrome,
        );
        assert_eq!(
            line,
            Line::from(vec![
                Span::raw("Compiling "),
                Span::styled("api", Style::new().underlined()),
            ])
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
    /// the key hints instead.
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
