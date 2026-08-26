//! Per-beam log storage: a bounded ring buffer with a following viewport.
//!
//! Each line is kept three ways — see [`LogLine`] — with the ANSI parsing
//! done once, at [`LogBuffer::push`], rather than on every render.
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

use alba_executors::Stream;
use ansi_to_tui::IntoText;
use ratatui::style::Style;

/// The memory bound a day-long watch session relies on.
pub const MAX_LINES: usize = 10_000;

/// One output line, kept three ways: `raw` as the command wrote it
/// (the exit replay prints it into a terminal), `text` with every
/// escape sequence removed (search, copy, wrapping widths, and the
/// `NO_COLOR` replay read this), and `spans` for the renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub raw: String,
    pub text: String,
    pub spans: Vec<(Style, String)>,
    pub stream: Stream,
    pub replayed: bool,
}

impl LogLine {
    fn new(raw: String, stream: Stream, replayed: bool) -> Self {
        let (text, spans) = parse(&raw);
        Self {
            raw,
            text,
            spans,
            stream,
            replayed,
        }
    }
}

/// One rendered row of the log pane: which buffer line it shows (`None`
/// for the truncation marker), which char range of that line, and that
/// slice's own text and spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub line: Option<usize>,
    pub chars: Range<usize>,
    pub spans: Vec<(Style, String)>,
    pub text: String,
}

impl Row {
    fn marker(truncated: usize) -> Self {
        let text = format!("… {truncated} older lines truncated");
        Self {
            line: None,
            chars: 0..0,
            spans: vec![(Style::default(), text.clone())],
            text,
        }
    }

    fn whole(index: usize, line: &LogLine) -> Self {
        Self {
            line: Some(index),
            chars: 0..line.text.chars().count(),
            spans: line.spans.clone(),
            text: line.text.clone(),
        }
    }
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
/// than `None`, plus a populated `sub_modifier` — a bookkeeping field
/// `patch` uses to compose styles, never read when a `Style` is
/// actually rendered. Left alone, a fully reset span would carry a
/// `Style` that renders identically to but does not `==`
/// [`Style::default`], which would both surprise a test asserting
/// equality and, later, confuse `logpane::patch`'s own use of
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

    /// While paused, an append must not move the pinned view: the tail
    /// moves on by one line regardless of whether this push also drops
    /// the oldest surviving line, so pinning the same *content* always
    /// needs the offset to grow by one to compensate. Truncation does
    /// not add a separate decrement of its own — it only clamps: once
    /// the offset would reach past the current top, it is capped there,
    /// so a reader paused anywhere (not only at the very top) stays
    /// pinned on the same lines across any number of incoming pushes,
    /// until truncation itself finally erases what they were looking at.
    pub fn push(&mut self, raw: impl Into<String>, stream: Stream, replayed: bool) {
        self.lines
            .push_back(LogLine::new(raw.into(), stream, replayed));
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

    /// The window of lines a pane of `height` rows shows, honoring the
    /// scroll state. When the window reaches the top of a truncated
    /// buffer, the marker `"… N older lines truncated"` is prepended as
    /// an extra row rather than substituted for one, so the reader is
    /// told without a real line being lost: the window then carries
    /// `height - 1` real lines plus the marker.
    pub fn view(&self, height: usize) -> Vec<Row> {
        let len = self.lines.len();
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = len.saturating_sub(offset);
        let start = end.saturating_sub(height);

        if start == 0 && self.truncated > 0 {
            let real_count = height.saturating_sub(1).min(end);
            std::iter::once(Row::marker(self.truncated))
                .chain(
                    self.lines
                        .iter()
                        .take(real_count)
                        .enumerate()
                        .map(|(index, line)| Row::whole(index, line)),
                )
                .collect()
        } else {
            self.lines
                .iter()
                .enumerate()
                .skip(start)
                .take(end - start)
                .map(|(index, line)| Row::whole(index, line))
                .collect()
        }
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
            buffer.push(format!("line {index}"), Stream::Stdout, false);
        }
        buffer
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    /// SGR colour becomes spans; the plain text loses every escape.
    #[test]
    fn a_coloured_line_is_split_into_styled_spans_and_plain_text() {
        let mut buffer = LogBuffer::new();
        buffer.push(
            "\u{1b}[1;31merror\u{1b}[0m: it broke",
            Stream::Stderr,
            false,
        );
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.raw, "\u{1b}[1;31merror\u{1b}[0m: it broke");
        assert_eq!(line.text, "error: it broke");
        assert_eq!(line.stream, Stream::Stderr);
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
        buffer.push(
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
        buffer.push("plain", Stream::Stdout, true);
        let line = buffer.lines().next().unwrap();
        assert_eq!(line.spans, vec![(Style::default(), "plain".to_string())]);
        assert!(line.replayed);
    }

    /// The view carries the spans and names the line each row shows.
    #[test]
    fn view_rows_name_their_line_and_carry_spans() {
        let mut buffer = LogBuffer::new();
        buffer.push("a", Stream::Stdout, false);
        buffer.push("\u{1b}[32mb\u{1b}[0m", Stream::Stdout, false);
        let rows = buffer.view(5);
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
        let rows = buffer.view(4);
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
        let view = buffer.view(4);
        assert_eq!(view[0].text, "… 5 older lines truncated");
        assert_eq!(view[1].text, "line 5");
    }

    /// Following by default: the view ends at the tail.
    #[test]
    fn a_following_buffer_shows_the_tail() {
        let buffer = filled(100);
        let view = buffer.view(3);
        assert_eq!(texts(&view), vec!["line 97", "line 98", "line 99"]);
    }

    /// Scrolling up suspends following; new lines no longer move the view.
    #[test]
    fn scrolling_up_pauses_and_pins_the_view() {
        let mut buffer = filled(100);
        buffer.scroll_up(10);
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 10 }));
        let pinned = buffer.view(3);
        buffer.push("line 100".to_string(), Stream::Stdout, false);
        assert_eq!(buffer.view(3), pinned, "a paused view does not move");
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
            buffer.push(format!("extra {index}"), Stream::Stdout, false);
        }
        let view = buffer.view(2);
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
        let pinned_rows = buffer.view(5);
        let pinned = texts(&pinned_rows);
        for index in 0..20 {
            buffer.push(format!("extra {index}"), Stream::Stdout, false); // every push truncates
        }
        let after_rows = buffer.view(5);
        assert_eq!(
            texts(&after_rows),
            pinned,
            "a mid-buffer pause must not drift toward the tail"
        );
    }

    #[test]
    fn a_zero_height_view_shows_nothing() {
        let buffer = filled(10);
        assert!(buffer.view(0).is_empty());
    }

    #[test]
    fn a_view_taller_than_the_buffer_shows_only_what_exists() {
        let buffer = filled(3);
        assert_eq!(texts(&buffer.view(10)), vec!["line 0", "line 1", "line 2"]);
    }

    #[test]
    fn an_empty_buffer_has_an_empty_view() {
        let buffer = LogBuffer::new();
        assert!(buffer.view(5).is_empty());
    }

    #[test]
    fn a_single_line_buffer_shows_that_line() {
        let buffer = filled(1);
        assert_eq!(texts(&buffer.view(3)), vec!["line 0"]);
    }

    /// `scroll_up` itself clamps the offset to the top, but `view` must
    /// also clamp a window whose start would otherwise fall below zero.
    #[test]
    fn a_window_wider_than_the_reachable_history_clamps_to_the_top() {
        let mut buffer = filled(100);
        buffer.scroll_up(1_000); // asks for more than exists above the tail
        assert_eq!(texts(&buffer.view(200)), vec!["line 0"]);
    }
}
