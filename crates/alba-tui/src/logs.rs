//! Per-beam log storage: a bounded ring buffer with a following viewport.
//!
//! Each line is kept three ways (see [`LogLine`]), with the ANSI parsing
//! done once, at [`LogBuffer::push_output`], rather than on every render.
//!
//! A day-long watch session can produce unbounded output, so the buffer
//! caps memory at [`MAX_LINES`] and remembers how many older lines it
//! dropped, so a reader is told rather than left wondering why the story
//! looks short. A rerun starts the beam's story fresh: [`LogBuffer::clear`]
//! resets the cap counter and the scroll position along with the lines.
//!
//! The reader controls the viewport independently of new output arriving:
//! scrolling up pauses following so new lines do not yank the view away,
//! and reaching the tail (or pressing `G`) resumes it.

use std::collections::VecDeque;
use std::ops::Range;

use alba_engine::NoticeLevel;
use alba_executors::Stream;
use ansi_to_tui::IntoText;
use ratatui::style::Style;
use unicode_width::UnicodeWidthChar;

/// The memory bound a day-long watch session relies on.
pub const MAX_LINES: usize = 10_000;

/// The cells Alba's notices and end lines are set in by: the `── ` that
/// opens their first row, and as much blank on every other row, so a
/// wrapped notice stays aligned under its own text.
pub const ALBA_LEAD: usize = 3;

/// Who wrote a line: a process (on one of its streams), or Alba itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// What a command printed.
    Output(Stream),
    /// The `$ command` echo before a command's output.
    Command,
    /// One of Alba's own notices about the beam.
    Notice(NoticeLevel),
    /// The line Alba writes when the beam ends.
    End(EndTone),
}

/// How an end line is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndTone {
    Success,
    Error,
    Cached,
    Muted,
}

/// Which process output the log pane shows. Alba's own lines (echo,
/// notices, end line) are never hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamFilter {
    #[default]
    All,
    Stdout,
    Stderr,
}

impl StreamFilter {
    /// `s`: all streams, then stdout only, then stderr only, then back.
    pub fn cycled(self) -> Self {
        match self {
            Self::All => Self::Stdout,
            Self::Stdout => Self::Stderr,
            Self::Stderr => Self::All,
        }
    }

    /// Whether `line` belongs to the filtered view.
    pub fn admits(self, line: &LogLine) -> bool {
        match (self, line.kind) {
            (Self::Stdout, LineKind::Output(stream)) => stream == Stream::Stdout,
            (Self::Stderr, LineKind::Output(stream)) => stream == Stream::Stderr,
            _ => true,
        }
    }
}

/// One output line, kept three ways: `raw` as the command wrote it
/// (the exit replay prints it into a terminal), `text` with every
/// escape sequence removed (search, copy, wrapping widths, and the
/// `NO_COLOR` replay read this), and `spans` for the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub raw: String,
    pub text: String,
    pub spans: Vec<(Style, String)>,
    pub kind: LineKind,
    /// The second or later physical line of a multi-line command or
    /// notice: drawn as part of the first, without its own decoration.
    pub continuation: bool,
    pub replayed: bool,
}

impl LogLine {
    fn output(raw: String, stream: Stream, replayed: bool) -> Self {
        let (text, spans) = parse(&raw);
        Self {
            raw,
            text,
            spans,
            kind: LineKind::Output(stream),
            continuation: false,
            replayed,
        }
    }

    /// A line Alba wrote itself: its text is shown as given, never parsed
    /// for escape sequences.
    fn alba(kind: LineKind, text: String, continuation: bool) -> Self {
        Self {
            raw: text.clone(),
            spans: vec![(Style::default(), text.clone())],
            text,
            kind,
            continuation,
            replayed: false,
        }
    }

    /// The cells drawn before this line's text on each of its rows.
    pub fn lead(&self) -> usize {
        match self.kind {
            LineKind::Notice(_) | LineKind::End(_) => ALBA_LEAD,
            LineKind::Output(_) | LineKind::Command => 0,
        }
    }

    /// The char ranges of this line's rows in a pane `width` cells wide,
    /// its lead taken off every row (width 0 still means no wrapping).
    fn ranges(&self, width: usize) -> Vec<Range<usize>> {
        let width = if width == 0 {
            0
        } else {
            width.saturating_sub(self.lead()).max(1)
        };
        wrap_ranges(&self.text, width)
    }
}

/// One rendered row of the log pane: which buffer line it shows (`None`
/// for the truncation marker), which char range of that line, and that
/// slice's own text and spans. `text` is, by construction, the
/// concatenation of `spans`' content in order; it exists so that search
/// and copy can match and slice against plain text without re-joining
/// `spans` themselves, not as a second, independent source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: Option<usize>,
    pub chars: Range<usize>,
    pub spans: Vec<(Style, String)>,
    pub text: String,
    /// The cells drawn before the text (see [`LogLine::lead`]).
    pub lead: usize,
}

impl Row {
    fn marker(truncated: usize) -> Self {
        let text = format!("… {truncated} older lines truncated");
        Self {
            line: None,
            chars: 0..0,
            spans: vec![(Style::default(), text.clone())],
            text,
            lead: 0,
        }
    }

    fn slice(index: usize, line: &LogLine, chars: Range<usize>) -> Self {
        let text: String = line
            .text
            .chars()
            .skip(chars.start)
            .take(chars.len())
            .collect();
        let mut spans = Vec::new();
        let mut at = 0;
        for (style, content) in &line.spans {
            let len = content.chars().count();
            let (span_start, span_end) = (at, at + len);
            at = span_end;
            let from = chars.start.max(span_start);
            let to = chars.end.min(span_end);
            if from < to {
                let piece: String = content
                    .chars()
                    .skip(from - span_start)
                    .take(to - from)
                    .collect();
                spans.push((*style, piece));
            }
        }
        if spans.is_empty() {
            spans.push((Style::default(), String::new()));
        }
        Self {
            line: Some(index),
            chars,
            spans,
            text,
            lead: line.lead(),
        }
    }
}

/// The char ranges of `text` that fit `width` cells each, by rendered
/// width (a `⚡` counts two). Width 0 means "do not wrap". An empty
/// line is one empty row, so it still occupies a row on screen.
// A single `0..count` here is a genuine one-row result, matching the
// `Vec<Range<usize>>` every other branch returns, not the "meant to
// collect the range's own values" case clippy's lint is meant to catch.
#[allow(clippy::single_range_in_vec_init)]
fn wrap_ranges(text: &str, width: usize) -> Vec<Range<usize>> {
    let count = text.chars().count();
    if width == 0 || count == 0 {
        return vec![0..count];
    }
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, c) in text.chars().enumerate() {
        let cells = c.width().unwrap_or(0).max(if c == '\t' { 1 } else { 0 });
        if used + cells > width && index > start {
            ranges.push(start..index);
            start = index;
            used = 0;
        }
        used += cells;
    }
    ranges.push(start..count);
    ranges
}

/// Splits `raw` into plain text and styled spans. Only SGR sequences
/// reach `ansi-to-tui`; everything else an escape can start (cursor
/// movement, erase, OSC titles and hyperlinks) is removed first, so
/// the parser's answer is deterministic and nothing ever shows raw.
fn parse(raw: &str) -> (String, Vec<(Style, String)>) {
    let sgr_only = keep_only_sgr(raw);
    let spans: Vec<(Style, String)> = match sgr_only.as_bytes().into_text() {
        Ok(text) => text
            .lines
            .into_iter()
            .flat_map(|line| line.spans)
            .filter(|span| !span.content.is_empty())
            .map(|span| (normalize(span.style), span.content.into_owned()))
            .collect(),
        Err(_) => vec![(Style::default(), sgr_only.replace('\u{1b}', ""))],
    };
    let spans = if spans.is_empty() {
        vec![(Style::default(), String::new())]
    } else {
        spans
    };
    let text = spans.iter().map(|(_, content)| content.as_str()).collect();
    (text, spans)
}

/// `ansi-to-tui` represents an SGR reset (`\x1b[0m`, or resetting just
/// one channel like `\x1b[39m`) as an explicit `Color::Reset` rather
/// than `None`, plus a populated `sub_modifier` (a bookkeeping field
/// `patch` uses to compose styles, never read when a `Style` is
/// actually rendered). Left alone, a fully reset span would carry a
/// `Style` that renders identically to but does not `==`
/// [`Style::default`], which would both surprise a test asserting
/// equality and, later, confuse `output::patch`'s own use of
/// `sub_modifier` when composing a search or selection highlight on
/// top. Collapsing `Color::Reset` to `None` and clearing
/// `sub_modifier` here makes every span carry the same canonical style
/// its rendering already implies.
fn normalize(mut style: Style) -> Style {
    if style.fg == Some(ratatui::style::Color::Reset) {
        style.fg = None;
    }
    if style.bg == Some(ratatui::style::Color::Reset) {
        style.bg = None;
    }
    if style.underline_color == Some(ratatui::style::Color::Reset) {
        style.underline_color = None;
    }
    style.sub_modifier = ratatui::style::Modifier::empty();
    style
}

/// Removes every escape sequence that is not an SGR (`ESC [ ... m`):
/// OSC (`ESC ] ... BEL` or `ESC ] ... ESC \`) and every other CSI
/// sequence (a final byte in `@..=~` other than `m`). A lone `ESC`
/// followed by anything else is dropped together with that character.
fn keep_only_sgr(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                let mut sequence = String::from("\u{1b}[");
                let mut final_byte = None;
                for c in chars.by_ref() {
                    sequence.push(c);
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                }
                if final_byte == Some('m') {
                    out.push_str(&sequence);
                }
            }
            Some(']') => {
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if c == '\u{7}' || (previous == '\u{1b}' && c == '\\') {
                        break;
                    }
                    previous = c;
                }
            }
            _ => {}
        }
    }
    out
}

/// The lines of `text`, at least one (an empty text is one empty line).
fn physical_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|line| line.trim_end_matches('\r'))
}

pub struct LogBuffer {
    lines: VecDeque<LogLine>,
    truncated: usize,
    scroll: Scroll,
}

/// `offset` counts lines above the tail, not an absolute index, so it
/// keeps meaning the same distance from "now" as the buffer grows.
pub enum Scroll {
    Following,
    Paused { offset: usize },
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            truncated: 0,
            scroll: Scroll::Following,
        }
    }

    /// A line a process wrote.
    pub fn push_output(&mut self, raw: impl Into<String>, stream: Stream, replayed: bool) {
        self.push_line(LogLine::output(raw.into(), stream, replayed));
    }

    /// The `$ command` echo. A command spanning several lines becomes one
    /// buffer line per physical line, the continuations indented, so no
    /// line ever holds a line break.
    pub fn push_command(&mut self, command: &str) {
        for (index, line) in physical_lines(command).enumerate() {
            let prefix = if index == 0 { "$ " } else { "  " };
            self.push_line(LogLine::alba(
                LineKind::Command,
                format!("{prefix}{line}"),
                index > 0,
            ));
        }
    }

    /// One of Alba's notices; a multi-line text becomes one line each.
    pub fn push_notice(&mut self, level: NoticeLevel, text: &str) {
        for (index, line) in physical_lines(text).enumerate() {
            self.push_line(LogLine::alba(
                LineKind::Notice(level),
                line.to_string(),
                index > 0,
            ));
        }
    }

    /// The line Alba writes when the beam ends.
    pub fn push_end(&mut self, tone: EndTone, text: &str) {
        self.push_line(LogLine::alba(LineKind::End(tone), text.to_string(), false));
    }

    /// While paused, an append must not move the pinned view: the tail
    /// moves on by one line regardless of whether this push also drops
    /// the oldest surviving line, so pinning the same *content* always
    /// needs the offset to grow by one to compensate. Truncation does
    /// not add a separate decrement of its own — it only clamps: once
    /// the offset would reach past the current top, it is capped there,
    /// so a reader paused anywhere (not only at the very top) stays
    /// pinned on the same lines across any number of incoming pushes,
    /// until truncation itself finally erases what they were looking at.
    fn push_line(&mut self, line: LogLine) {
        self.lines.push_back(line);
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.truncated += 1;
        }
        if let Scroll::Paused { offset } = &mut self.scroll {
            let max_offset = self.lines.len().saturating_sub(1);
            *offset = (*offset + 1).min(max_offset);
        }
    }

    /// Rerun: the beam's story starts fresh, so the cap counter and the
    /// scroll position reset along with the lines.
    pub fn clear(&mut self) {
        self.lines.clear();
        self.truncated = 0;
        self.scroll = Scroll::Following;
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// How many old lines were dropped past the cap.
    pub fn truncated(&self) -> usize {
        self.truncated
    }

    pub fn lines(&self) -> impl Iterator<Item = &LogLine> {
        self.lines.iter()
    }

    /// The single line at `index`, or `None` past the end of the
    /// buffer. `VecDeque::get` is O(1), the natural home for the lookup
    /// `copy::line_text` and `output::full_line` each used to spell out
    /// by hand as `lines().nth(index)`.
    pub fn line(&self, index: usize) -> Option<&LogLine> {
        self.lines.get(index)
    }

    pub fn scroll(&self) -> &Scroll {
        &self.scroll
    }

    /// Pauses following: the view stays pinned where the reader left it.
    pub fn scroll_up(&mut self, by: usize) {
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let max_offset = self.lines.len().saturating_sub(1);
        self.scroll = Scroll::Paused {
            offset: (offset + by).min(max_offset),
        };
    }

    /// Reaching the tail resumes following.
    pub fn scroll_down(&mut self, by: usize) {
        if let Scroll::Paused { offset } = self.scroll {
            let offset = offset.saturating_sub(by);
            self.scroll = if offset == 0 {
                Scroll::Following
            } else {
                Scroll::Paused { offset }
            };
        }
    }

    /// The `G` key: jump straight back to following, wherever the reader
    /// had scrolled to.
    pub fn follow_tail(&mut self) {
        self.scroll = Scroll::Following;
    }

    /// The rows a pane of `height` by `width` shows through `filter`
    /// (hidden lines have no rows; line indices stay buffer-absolute),
    /// honoring the scroll state: the last `height` rows of the lines up
    /// to the tail minus the paused offset, the top line cut to its last
    /// rows when it does not fit whole. When the first visible line's
    /// first row is in view and lines were dropped past the cap, the
    /// marker `"… N older lines truncated"` is prepended as a row of its
    /// own, displacing the bottom row.
    pub fn view(&self, height: usize, width: usize, filter: StreamFilter) -> Vec<Row> {
        if height == 0 {
            return Vec::new();
        }
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = self.lines.len().saturating_sub(offset);
        let mut rows: VecDeque<Row> = VecDeque::with_capacity(height);
        let mut line = end;
        while line > 0 && rows.len() < height {
            line -= 1;
            let text = &self.lines[line];
            if !filter.admits(text) {
                continue;
            }
            for range in text.ranges(width).into_iter().rev() {
                if rows.len() == height {
                    break;
                }
                rows.push_front(Row::slice(line, text, range));
            }
        }
        let first_visible = self.lines.iter().position(|line| filter.admits(line));
        let at_top = rows.front().is_some_and(|row| {
            row.line.is_some() && row.line == first_visible && row.chars.start == 0
        });
        if at_top && self.truncated > 0 {
            if rows.len() == height {
                rows.pop_back();
            }
            rows.push_front(Row::marker(self.truncated));
        }
        rows.into()
    }

    /// Where the rows `view` shows sit among all the rows the filtered
    /// buffer renders as: the window's row range and the total, the
    /// truncation marker counted as a row. Feeds the scrollbar.
    pub fn row_window(
        &self,
        height: usize,
        width: usize,
        filter: StreamFilter,
    ) -> (Range<usize>, usize) {
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = self.lines.len().saturating_sub(offset);
        let rows = |range: Range<usize>| -> usize {
            self.lines
                .range(range)
                .filter(|line| filter.admits(line))
                .map(|line| line.ranges(width).len())
                .sum()
        };
        let total = usize::from(self.truncated > 0) + rows(0..self.lines.len());
        let window_end = total - rows(end..self.lines.len());
        let shown = self.view(height, width, filter).len();
        (window_end.saturating_sub(shown)..window_end, total)
    }

    /// `PgUp`, `Ctrl-u`, the wheel: moves the window up by whole lines
    /// covering at least `rows` rendered rows, so a page never lands
    /// mid-line and always travels at least as far as asked.
    pub fn scroll_up_rows(&mut self, rows: usize, width: usize, filter: StreamFilter) {
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let mut index = self.lines.len().saturating_sub(offset);
        let mut covered = 0;
        let mut moved = 0;
        while index > 0 && covered < rows {
            index -= 1;
            moved += 1;
            if filter.admits(&self.lines[index]) {
                covered += self.lines[index].ranges(width).len();
            }
        }
        self.scroll_up(moved.max(1));
    }

    /// The other way; reaching the tail resumes following, as `scroll_down` does.
    pub fn scroll_down_rows(&mut self, rows: usize, width: usize, filter: StreamFilter) {
        let Scroll::Paused { offset } = self.scroll else {
            return;
        };
        let mut index = self.lines.len().saturating_sub(offset);
        let mut covered = 0;
        let mut moved = 0;
        while index < self.lines.len() && covered < rows {
            if filter.admits(&self.lines[index]) {
                covered += self.lines[index].ranges(width).len();
            }
            index += 1;
            moved += 1;
        }
        self.scroll_down(moved.max(1));
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_executors::Stream;
    use ratatui::style::{Color, Modifier, Style};

    fn filled(count: usize) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        for index in 0..count {
            buffer.push_output(format!("line {index}"), Stream::Stdout, false);
        }
        buffer
    }

    /// The scrollbar counts rows, not lines: a line wrapped in two rows
    /// above the window moves it down by two.
    #[test]
    fn the_row_window_counts_wrapped_rows() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("abcdefgh", Stream::Stdout, false);
        for index in 1..10 {
            buffer.push_output(format!("l{index}"), Stream::Stdout, false);
        }
        assert_eq!(buffer.row_window(4, 4, StreamFilter::All), (7..11, 11));
        buffer.scroll_up(3);
        assert_eq!(buffer.row_window(4, 4, StreamFilter::All), (4..8, 11));
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    /// SGR colour becomes spans; the plain text loses every escape.
    #[test]
    fn a_coloured_line_is_split_into_styled_spans_and_plain_text() {
        let mut buffer = LogBuffer::new();
        buffer.push_output(
            "\u{1b}[1;31merror\u{1b}[0m: it broke",
            Stream::Stderr,
            false,
        );
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.raw, "\u{1b}[1;31merror\u{1b}[0m: it broke");
        assert_eq!(line.text, "error: it broke");
        assert_eq!(line.kind, LineKind::Output(Stream::Stderr));
        assert_eq!(
            line.spans,
            vec![
                (
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    "error".to_string()
                ),
                (Style::default(), ": it broke".to_string()),
            ]
        );
    }

    /// Cursor movement, erase, and OSC sequences carry no text: dropped,
    /// never shown raw.
    #[test]
    fn non_sgr_sequences_are_dropped() {
        let mut buffer = LogBuffer::new();
        buffer.push_output(
            "\u{1b}[2K\u{1b}]0;title\u{7}hello \u{1b}[1Aworld\u{1b}]8;;http://x\u{1b}\\",
            Stream::Stdout,
            false,
        );
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.text, "hello world");
        assert_eq!(
            line.spans,
            vec![(Style::default(), "hello world".to_string())]
        );
    }

    /// A line with no escapes at all is one default span.
    #[test]
    fn a_plain_line_is_one_default_span() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("plain", Stream::Stdout, true);
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.spans, vec![(Style::default(), "plain".to_string())]);
        assert!(line.replayed);
    }

    /// The view carries the spans and names the line each row shows.
    #[test]
    fn view_rows_name_their_line_and_carry_spans() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("a", Stream::Stdout, false);
        buffer.push_output("\u{1b}[32mb\u{1b}[0m", Stream::Stdout, false);
        let rows = buffer.view(5, 80, StreamFilter::All);
        assert_eq!(rows[0].line, Some(0));
        assert_eq!(rows[0].chars, 0..1);
        assert_eq!(rows[1].line, Some(1));
        assert_eq!(rows[1].text, "b");
        assert_eq!(
            rows[1].spans,
            vec![(Style::default().fg(Color::Green), "b".to_string())]
        );
    }

    /// The marker row names no line.
    #[test]
    fn the_marker_row_names_no_line() {
        let mut buffer = filled(MAX_LINES + 5);
        buffer.scroll_up(MAX_LINES);
        let rows = buffer.view(4, 80, StreamFilter::All);
        assert_eq!(rows[0].line, None);
        assert_eq!(rows[0].text, "… 5 older lines truncated");
        assert_eq!(rows[1].line, Some(0));
        assert_eq!(rows[1].text, "line 5");
    }

    /// The cap is the memory bound a day-long watch session relies on.
    #[test]
    fn the_buffer_drops_oldest_lines_past_the_cap() {
        let buffer = filled(MAX_LINES + 5);
        assert_eq!(buffer.len(), MAX_LINES);
        assert_eq!(buffer.truncated(), 5);
        assert_eq!(buffer.lines().next().unwrap().text, "line 5");
    }

    /// The marker is a rendered line, so the reader learns lines are gone.
    #[test]
    fn the_view_announces_truncation_at_the_top() {
        let mut buffer = filled(MAX_LINES + 5);
        buffer.scroll_up(MAX_LINES); // all the way to the top
        let view = buffer.view(4, 80, StreamFilter::All);
        assert_eq!(view[0].text, "… 5 older lines truncated");
        assert_eq!(view[1].text, "line 5");
    }

    /// Following by default: the view ends at the tail.
    #[test]
    fn a_following_buffer_shows_the_tail() {
        let buffer = filled(100);
        let view = buffer.view(3, 80, StreamFilter::All);
        assert_eq!(texts(&view), vec!["line 97", "line 98", "line 99"]);
    }

    /// Scrolling up suspends following; new lines no longer move the view.
    #[test]
    fn scrolling_up_pauses_and_pins_the_view() {
        let mut buffer = filled(100);
        buffer.scroll_up(10);
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 10 }));
        let pinned = buffer.view(3, 80, StreamFilter::All);
        buffer.push_output("line 100".to_string(), Stream::Stdout, false);
        assert_eq!(
            buffer.view(3, 80, StreamFilter::All),
            pinned,
            "a paused view does not move"
        );
    }

    /// Scrolling back to the bottom resumes following, as does `G`.
    #[test]
    fn reaching_the_tail_resumes_following() {
        let mut buffer = filled(100);
        buffer.scroll_up(3);
        buffer.scroll_down(3);
        assert!(matches!(buffer.scroll(), Scroll::Following));
        buffer.scroll_up(50);
        buffer.follow_tail();
        assert!(matches!(buffer.scroll(), Scroll::Following));
    }

    /// A rerun starts a fresh story: buffer, truncation count, and scroll
    /// all reset.
    #[test]
    fn clear_resets_everything() {
        let mut buffer = filled(MAX_LINES + 5);
        buffer.scroll_up(7);
        buffer.clear();
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.truncated(), 0);
        assert!(matches!(buffer.scroll(), Scroll::Following));
    }

    /// A paused offset never dangles past the top when old lines drop.
    #[test]
    fn truncation_clamps_a_paused_offset() {
        let mut buffer = filled(MAX_LINES);
        buffer.scroll_up(MAX_LINES); // paused at the very top
        for index in 0..10 {
            buffer.push_output(format!("extra {index}"), Stream::Stdout, false);
        }
        let view = buffer.view(2, 80, StreamFilter::All);
        assert_eq!(view[0].text, "… 10 older lines truncated");
    }

    /// A pause well below the very top — the realistic case in a
    /// day-long over-cap session — must stay pinned across incoming
    /// lines exactly like a pause at the top does. Truncation only ever
    /// clamps the reachable top; it must not silently drift the window
    /// toward the tail on every incoming, truncating push.
    #[test]
    fn a_mid_buffer_pause_stays_pinned_across_truncating_pushes() {
        let mut buffer = filled(MAX_LINES); // already at the cap
        buffer.scroll_up(MAX_LINES / 2); // paused halfway up, nowhere near the top
        // Compared by text, not by whole `Row`: `Row::line` is a
        // deque-relative position (the same addressing `copy::CopyState`
        // already uses), so it legitimately shifts by one per truncating
        // push even while the pinned *content* does not move at all.
        let pinned_rows = buffer.view(5, 80, StreamFilter::All);
        let pinned = texts(&pinned_rows);
        for index in 0..20 {
            buffer.push_output(format!("extra {index}"), Stream::Stdout, false); // every push truncates
        }
        let after_rows = buffer.view(5, 80, StreamFilter::All);
        assert_eq!(
            texts(&after_rows),
            pinned,
            "a mid-buffer pause must not drift toward the tail"
        );
    }

    #[test]
    fn a_zero_height_view_shows_nothing() {
        let buffer = filled(10);
        assert!(buffer.view(0, 80, StreamFilter::All).is_empty());
    }

    #[test]
    fn a_view_taller_than_the_buffer_shows_only_what_exists() {
        let buffer = filled(3);
        assert_eq!(
            texts(&buffer.view(10, 80, StreamFilter::All)),
            vec!["line 0", "line 1", "line 2"]
        );
    }

    #[test]
    fn an_empty_buffer_has_an_empty_view() {
        let buffer = LogBuffer::new();
        assert!(buffer.view(5, 80, StreamFilter::All).is_empty());
    }

    #[test]
    fn a_single_line_buffer_shows_that_line() {
        let buffer = filled(1);
        assert_eq!(
            texts(&buffer.view(3, 80, StreamFilter::All)),
            vec!["line 0"]
        );
    }

    /// `scroll_up` itself clamps the offset to the top, but `view` must
    /// also clamp a window whose start would otherwise fall below zero.
    #[test]
    fn a_window_wider_than_the_reachable_history_clamps_to_the_top() {
        let mut buffer = filled(100);
        buffer.scroll_up(1_000); // asks for more than exists above the tail
        assert_eq!(
            texts(&buffer.view(200, 80, StreamFilter::All)),
            vec!["line 0"]
        );
    }

    /// A line wider than the pane wraps by character; the rows name the
    /// same line and consecutive char ranges.
    #[test]
    fn a_long_line_wraps_into_consecutive_rows() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("abcdefghij", Stream::Stdout, false);
        let rows = buffer.view(5, 4, StreamFilter::All);
        assert_eq!(texts(&rows), vec!["abcd", "efgh", "ij"]);
        assert_eq!(rows[1].line, Some(0));
        assert_eq!(rows[1].chars, 4..8);
        assert_eq!(rows[2].chars, 8..10);
    }

    /// A double-width char counts two cells, so it never straddles a row.
    #[test]
    fn wrapping_counts_rendered_width() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("ab⚡cd", Stream::Stdout, false);
        assert_eq!(
            texts(&buffer.view(5, 3, StreamFilter::All)),
            vec!["ab", "⚡c", "d"]
        );
    }

    /// Following shows the last `height` rows, cutting the top line if
    /// it does not fit whole.
    #[test]
    fn following_shows_the_last_rows_even_mid_line() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("aaaaaaaa", Stream::Stdout, false);
        buffer.push_output("bb", Stream::Stdout, false);
        let rows = buffer.view(2, 4, StreamFilter::All);
        assert_eq!(texts(&rows), vec!["aaaa", "bb"]);
        assert_eq!(rows[0].chars, 4..8);
    }

    /// The marker is never wrapped and still displaces the bottom row.
    ///
    /// Scrolling up by `MAX_LINES` itself clamps to the very top, where
    /// only the single oldest surviving line is reachable (see
    /// `a_window_wider_than_the_reachable_history_clamps_to_the_top`),
    /// too little to exercise displacement at all. This pins the offset
    /// so exactly `height` real lines (indices 0..3) are reachable
    /// instead, which is what makes the marker's insertion actually
    /// have to evict the newest of them to keep the row count at
    /// `height`.
    #[test]
    fn the_marker_stays_one_row_at_the_top() {
        let mut buffer = filled(MAX_LINES + 5);
        buffer.scroll_up(MAX_LINES - 3);
        let rows = buffer.view(3, 6, StreamFilter::All);
        assert_eq!(rows[0].line, None);
        assert_eq!(texts(&rows)[1..], ["line 5", "line 6"]);
    }

    /// The offset is in lines: the same line stays pinned at two widths.
    #[test]
    fn a_paused_offset_pins_the_same_line_at_any_width() {
        let mut buffer = filled(100);
        buffer.scroll_up(10);
        let narrow = buffer.view(3, 4, StreamFilter::All);
        let wide = buffer.view(3, 80, StreamFilter::All);
        assert_eq!(narrow.last().unwrap().line, wide.last().unwrap().line);
        assert_eq!(wide.last().unwrap().text, "line 89");
    }

    /// An empty line is one empty row; width 0 never wraps.
    #[test]
    fn empty_lines_and_zero_width_are_one_row() {
        let mut buffer = LogBuffer::new();
        buffer.push_output("", Stream::Stdout, false);
        buffer.push_output("abc", Stream::Stdout, false);
        assert_eq!(
            texts(&buffer.view(5, 2, StreamFilter::All)),
            vec!["", "ab", "c"]
        );
        assert_eq!(
            texts(&buffer.view(5, 0, StreamFilter::All)),
            vec!["", "abc"]
        );
    }

    /// Row scrolling moves by whole lines, at least as far as asked.
    #[test]
    fn row_scrolling_rounds_toward_the_direction_of_travel() {
        let mut buffer = LogBuffer::new();
        for _ in 0..5 {
            buffer.push_output("aaaaaaaa", Stream::Stdout, false); // 2 rows each at width 4
        }
        buffer.scroll_up_rows(3, 4, StreamFilter::All); // 3 rows up: covers 2 lines
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 2 }));
        buffer.scroll_down_rows(1, 4, StreamFilter::All); // 1 row down: one whole line
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 1 }));
        buffer.scroll_down_rows(1, 4, StreamFilter::All);
        assert!(matches!(buffer.scroll(), Scroll::Following));
    }

    /// A command or notice never holds a line break: each physical line
    /// is its own buffer line, continuations indented for commands.
    #[test]
    fn multi_line_commands_and_notices_split_into_lines() {
        let mut buffer = LogBuffer::new();
        buffer.push_command("echo a\necho b");
        buffer.push_notice(NoticeLevel::Error, "first\r\nsecond");
        let lines: Vec<(&str, LineKind, bool)> = buffer
            .lines()
            .map(|line| (line.text.as_str(), line.kind, line.continuation))
            .collect();
        assert_eq!(
            lines,
            vec![
                ("$ echo a", LineKind::Command, false),
                ("  echo b", LineKind::Command, true),
                ("first", LineKind::Notice(NoticeLevel::Error), false),
                ("second", LineKind::Notice(NoticeLevel::Error), true),
            ]
        );
    }

    /// Scrolling counts the lines the reader sees, not the hidden ones.
    #[test]
    fn filtered_scrolling_moves_over_visible_lines() {
        let mut buffer = LogBuffer::new();
        for index in 0..4 {
            buffer.push_output(format!("out {index}"), Stream::Stdout, false);
            buffer.push_output(format!("err {index}"), Stream::Stderr, false);
        }
        buffer.scroll_up_rows(2, 80, StreamFilter::Stdout);
        assert_eq!(
            texts(&buffer.view(2, 80, StreamFilter::Stdout)),
            vec!["out 0", "out 1"]
        );
        buffer.scroll_down_rows(1, 80, StreamFilter::Stdout);
        assert_eq!(
            texts(&buffer.view(2, 80, StreamFilter::Stdout)),
            vec!["out 1", "out 2"]
        );
    }
}
