//! Copy mode: a selection over the selected beam's log buffer, and
//! getting that text onto the user's clipboard.
//!
//! This module is pure except for [`copy_to_clipboard`]'s two calls out
//! to the world (writing an escape sequence to the terminal, asking
//! `arboard` for the system clipboard) — the selection model
//! ([`CopyState`]) and the OSC 52 encoding ([`osc52`], [`base64`]) never
//! touch anything outside their own arguments, so they are tested as
//! plain functions.
//!
//! The selection is addressed in `(line, column)` coordinates over the
//! log buffer — pane-aware by construction, since that buffer holds
//! nothing but log text, never the tree beside it. Columns count
//! *characters*, not bytes: a log line is arbitrary process output, and
//! slicing it at a byte offset that lands mid-codepoint would panic. All
//! the slicing below goes through [`char`] iteration for exactly that
//! reason.

use crate::logs::LogBuffer;

/// A selection over the selected beam's buffer. `anchor` is where the
/// selection started (`v`, or a mouse-down); `cursor` is where it
/// currently ends — whichever of the two comes first, in `(line,
/// column)` order, is the start of the span `selected_text` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyState {
    pub anchor: (usize, usize),
    pub cursor: (usize, usize),
}

impl CopyState {
    /// `v`: a fresh selection anchored at the pane's top visible line,
    /// column 0 — the one point on screen that is always there to start
    /// from, regardless of where the reader has scrolled to.
    pub fn new_at(line: usize) -> Self {
        Self {
            anchor: (line, 0),
            cursor: (line, 0),
        }
    }

    /// Moves the cursor by `dl` lines and `dc` columns, clamped to the
    /// buffer: the line never leaves `0..buffer.len()`, and the column
    /// never leaves the char range the landed-on line actually has —
    /// moving onto a shorter line pulls the column back to its last
    /// character (or to 0, on an empty one) the way a normal editor's
    /// cursor does.
    pub fn move_cursor(&mut self, dl: isize, dc: isize, buffer: &LogBuffer) {
        let max_line = buffer.len().saturating_sub(1);
        let line = clamp_by(self.cursor.0, dl, max_line);
        let max_col = char_count(buffer, line).saturating_sub(1);
        let column = clamp_by(self.cursor.1, dc, max_col);
        self.cursor = (line, column);
    }

    /// The text the selection covers, `\n`-joined: a character span
    /// from the earlier of `anchor`/`cursor` to the later one, inclusive
    /// at both ends, however the user dragged. A single-line span is
    /// just that line's own inclusive slice; a multi-line one runs from
    /// the start column to the end of the first line, keeps every line
    /// strictly between whole, and ends with the last line's own start
    /// up to (and including) the end column.
    pub fn selected_text(&self, buffer: &LogBuffer) -> String {
        let (start, end) = self.normalized();
        if start.0 == end.0 {
            let line = line_text(buffer, start.0);
            return slice_chars(&line, start.1, end.1.saturating_add(1));
        }
        let mut parts = Vec::with_capacity(end.0 - start.0 + 1);
        let first = line_text(buffer, start.0);
        let first_len = first.chars().count();
        parts.push(slice_chars(&first, start.1, first_len));
        for index in (start.0 + 1)..end.0 {
            parts.push(line_text(buffer, index));
        }
        let last = line_text(buffer, end.0);
        parts.push(slice_chars(&last, 0, end.1.saturating_add(1)));
        parts.join("\n")
    }

    /// The inclusive character-column range this selection covers on
    /// `line`, given that line's own char count — or `None` when `line`
    /// falls outside the (normalized) span altogether, or is empty.
    /// Used by `ui/logpane.rs` to style the covered spans; kept here
    /// rather than there because it is the same normalize-then-clip
    /// logic `selected_text` already does, just answering "which
    /// columns" instead of "which text".
    pub fn covers_line(&self, line: usize, line_char_len: usize) -> Option<(usize, usize)> {
        if line_char_len == 0 {
            return None;
        }
        let (start, end) = self.normalized();
        if line < start.0 || line > end.0 {
            return None;
        }
        let from = if line == start.0 { start.1 } else { 0 };
        let to = if line == end.0 {
            end.1
        } else {
            line_char_len - 1
        };
        let from = from.min(line_char_len - 1);
        let to = to.min(line_char_len - 1);
        (from <= to).then_some((from, to))
    }

    /// `anchor`/`cursor` in `(line, column)` order rather than in
    /// drag order — the same normalization every method here needs
    /// before it can clip against a span, since the user may have
    /// dragged either way.
    fn normalized(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

fn clamp_by(current: usize, delta: isize, max: usize) -> usize {
    (current as isize + delta).clamp(0, max as isize) as usize
}

/// The buffer's line at `index`, or empty when there is no such line —
/// a selection can point past the end of a buffer that just got shorter
/// (a rerun clearing it out from under an open copy-mode session), and
/// that must read as nothing rather than panic.
fn line_text(buffer: &LogBuffer, index: usize) -> String {
    buffer
        .line(index)
        .map(|line| line.text.clone())
        .unwrap_or_default()
}

fn char_count(buffer: &LogBuffer, index: usize) -> usize {
    line_text(buffer, index).chars().count()
}

/// `text[from..to)` by *character* index, not byte offset — safe against
/// any non-ASCII content and never panics, since both bounds are clamped
/// by `Iterator::skip`/`take` rather than by string slicing.
fn slice_chars(text: &str, from: usize, to: usize) -> String {
    if to <= from {
        return String::new();
    }
    text.chars().skip(from).take(to - from).collect()
}

/// The buffer-absolute index of the first line with a row on screen
/// (possibly a partial one: the top line cut to its last rows).
/// Clamped to the buffer's own last line for a zero-height pane.
pub fn top_visible_line(buffer: &LogBuffer, height: usize, width: usize) -> usize {
    scroll_window(buffer, height, width)
        .0
        .min(buffer.len().saturating_sub(1))
}

/// Translates a row of the log pane's content area into the buffer
/// line it shows and the char index that row starts at, or `None` on
/// the marker row or below the last rendered row. The mouse hit test
/// and the highlights both go through this, so a click always lands
/// on the line the highlight would mark.
pub fn row_at(
    buffer: &LogBuffer,
    height: usize,
    width: usize,
    pane_row: usize,
) -> Option<(usize, usize)> {
    let rows = buffer.view(height, width);
    let row = rows.get(pane_row)?;
    Some((row.line?, row.chars.start))
}

/// The `(start, end)` line window `LogBuffer::view` currently draws
/// from: `start` is the first line with a visible row, `end` one past
/// the last. `(0, 0)` for an empty view.
pub fn scroll_window(buffer: &LogBuffer, height: usize, width: usize) -> (usize, usize) {
    let rows = buffer.view(height, width);
    let mut lines = rows.iter().filter_map(|row| row.line);
    match (lines.next(), lines.next_back()) {
        (Some(first), Some(last)) => (first, last + 1),
        (Some(only), None) => (only, only + 1),
        (None, _) => (0, 0),
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The standard base64 alphabet, implemented locally rather than adding
/// a dependency for the one call OSC 52 needs.
fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[((n >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// The OSC 52 escape that puts `text` on the terminal's clipboard
/// (target `c`) — the one way a *remote* session can still reach the
/// user's local clipboard, since the terminal emulator itself, not the
/// remote shell, is what intercepts the sequence. `None` when the
/// base64 payload would exceed 100 000 bytes: an escape that large is
/// not sane to emit down what may be a slow link.
pub fn osc52(text: &str) -> Option<String> {
    let payload = base64(text.as_bytes());
    if payload.len() > 100_000 {
        return None;
    }
    Some(format!("\u{1b}]52;c;{payload}\u{7}"))
}

/// Best effort, in order: OSC 52 written straight to the terminal
/// (raw mode passes it through untouched), then `arboard`'s system
/// clipboard. `arboard` failing — headless, no clipboard daemon to talk
/// to — is not an error, since OSC 52 already went out; this returns
/// whichever is the strongest thing that actually worked, for the
/// status line to report.
pub fn copy_to_clipboard(text: &str) -> &'static str {
    use std::io::Write;

    let osc_written = osc52(text).is_some_and(|sequence| {
        let mut stdout = std::io::stdout();
        stdout
            .write_all(sequence.as_bytes())
            .and_then(|()| stdout.flush())
            .is_ok()
    });
    let system_written = arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
        .is_ok();

    match (osc_written, system_written) {
        (true, _) => "copied (OSC 52)",
        (false, true) => "copied (system)",
        (false, false) => "copy failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_of(lines: &[&str]) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        for text in lines {
            buffer.push(text.to_string(), alba_executors::Stream::Stdout, false);
        }
        buffer
    }

    #[test]
    fn a_selection_spans_lines_inclusively() {
        let buffer = buffer_of(&["alpha", "bravo", "charlie"]);
        let mut copy = CopyState::new_at(0);
        copy.cursor = (1, 2);
        assert_eq!(copy.selected_text(&buffer), "alpha\nbra");
    }

    /// The controller's ruling on the brief's contradiction: end-column
    /// inclusive semantics govern throughout, so a backwards drag whose
    /// cursor lands at column 0 selects only that one character on its
    /// last line.
    #[test]
    fn a_backwards_selection_normalizes() {
        let buffer = buffer_of(&["alpha", "bravo"]);
        let mut copy = CopyState::new_at(1);
        copy.cursor = (0, 0);
        assert_eq!(copy.selected_text(&buffer), "alpha\nb");
    }

    #[test]
    fn a_same_line_selection_is_the_inclusive_slice() {
        let buffer = buffer_of(&["Compiling api v0.1.0"]);
        let mut copy = CopyState::new_at(0);
        copy.cursor = (0, 3);
        assert_eq!(copy.selected_text(&buffer), "Comp");
    }

    /// Multi-byte characters must never cause a panic, and columns
    /// address *characters* — the accented letters count as one column
    /// each, not two or three bytes.
    #[test]
    fn selection_columns_count_characters_not_bytes() {
        let buffer = buffer_of(&["héllo wörld"]);
        let mut copy = CopyState::new_at(0);
        copy.cursor = (0, 3);
        assert_eq!(copy.selected_text(&buffer), "héll");
    }

    #[test]
    fn move_cursor_clamps_to_the_buffer() {
        let buffer = buffer_of(&["ab", "cdef"]);
        let mut copy = CopyState::new_at(0);
        copy.move_cursor(-5, -5, &buffer);
        assert_eq!(copy.cursor, (0, 0));
        copy.move_cursor(5, 5, &buffer);
        assert_eq!(
            copy.cursor,
            (1, 3),
            "clamped to the last line and its last char"
        );
        copy.move_cursor(-1, 0, &buffer);
        assert_eq!(
            copy.cursor,
            (0, 1),
            "moving onto the shorter line pulls the column back to its last char"
        );
    }

    #[test]
    fn move_cursor_on_an_empty_buffer_does_not_panic() {
        let buffer = LogBuffer::new();
        let mut copy = CopyState::new_at(0);
        copy.move_cursor(3, 3, &buffer);
        assert_eq!(copy.cursor, (0, 0));
    }

    #[test]
    fn covers_line_is_none_outside_the_span() {
        let mut copy = CopyState::new_at(1);
        copy.cursor = (2, 2);
        assert_eq!(copy.covers_line(0, 5), None, "before the span");
        assert_eq!(copy.covers_line(3, 5), None, "after the span");
        assert_eq!(
            copy.covers_line(1, 5),
            Some((0, 4)),
            "first line: to its end"
        );
        assert_eq!(
            copy.covers_line(2, 5),
            Some((0, 2)),
            "last line: up to the end column"
        );
    }

    #[test]
    fn covers_line_on_an_empty_line_is_none() {
        let copy = CopyState::new_at(0);
        assert_eq!(copy.covers_line(0, 0), None);
    }

    #[test]
    fn base64_encodes_hi() {
        assert_eq!(base64(b"hi"), "aGk=");
    }

    #[test]
    fn base64_encodes_hiya() {
        assert_eq!(base64(b"hiya"), "aGl5YQ==");
    }

    /// A length that is a multiple of 3 needs no padding at all — the
    /// third and fourth output chars of every chunk are always real,
    /// unlike the one- and two-byte tail cases above.
    #[test]
    fn base64_needs_no_padding_when_the_length_is_a_multiple_of_three() {
        assert_eq!(base64(b"abc"), "YWJj");
    }

    /// The exact escape shape: OSC 52, clipboard target `c`, base64
    /// payload, BEL terminator.
    #[test]
    fn osc52_encodes_the_selection() {
        assert_eq!(osc52("hi"), Some("\u{1b}]52;c;aGk=\u{7}".to_string()));
    }

    /// Base64 inflates by 4/3, so 80 000 raw bytes cross the 100 000
    /// byte ceiling on the encoded payload.
    #[test]
    fn osc52_refuses_a_payload_too_large_to_emit() {
        let huge = "a".repeat(80_000);
        assert_eq!(osc52(&huge), None);
    }

    #[test]
    fn top_visible_line_follows_the_tail_by_default() {
        let mut buffer = LogBuffer::new();
        for index in 0..10 {
            buffer.push(
                format!("line {index}"),
                alba_executors::Stream::Stdout,
                false,
            );
        }
        assert_eq!(top_visible_line(&buffer, 3, 80), 7);
    }

    #[test]
    fn top_visible_line_honors_a_paused_scroll() {
        let mut buffer = LogBuffer::new();
        for index in 0..10 {
            buffer.push(
                format!("line {index}"),
                alba_executors::Stream::Stdout,
                false,
            );
        }
        buffer.scroll_up(4);
        assert_eq!(top_visible_line(&buffer, 3, 80), 3);
    }

    #[test]
    fn row_at_maps_rows_to_buffer_lines() {
        let mut buffer = LogBuffer::new();
        for index in 0..10 {
            buffer.push(
                format!("line {index}"),
                alba_executors::Stream::Stdout,
                false,
            );
        }
        // Following, height 3: rows show lines 7, 8, 9.
        assert_eq!(row_at(&buffer, 3, 80, 0), Some((7, 0)));
        assert_eq!(row_at(&buffer, 3, 80, 2), Some((9, 0)));
        assert_eq!(
            row_at(&buffer, 3, 80, 3),
            None,
            "past what the view actually rendered"
        );
    }

    /// The truncation-marker row (present once the buffer has dropped
    /// old lines and the view reaches the very top) names no buffer
    /// line: row 0 is the marker, row 1 is the first real line.
    #[test]
    fn row_at_skips_the_truncation_marker() {
        let mut buffer = LogBuffer::new();
        for index in 0..(crate::logs::MAX_LINES + 5) {
            buffer.push(
                format!("line {index}"),
                alba_executors::Stream::Stdout,
                false,
            );
        }
        buffer.scroll_up(crate::logs::MAX_LINES); // paused at the very top
        assert_eq!(
            row_at(&buffer, 4, 80, 0),
            None,
            "row 0 is the marker, not a line"
        );
        // Indices address the buffer's own current storage, oldest
        // surviving line first — not the numbers baked into the pushed
        // text, which is why this is 0 rather than 5 (`buffer.lines()`
        // itself starts over at 0 once the oldest 5 lines are dropped).
        assert_eq!(
            row_at(&buffer, 4, 80, 1),
            Some((0, 0)),
            "the first surviving line"
        );
    }

    /// A wrapped line's second row reports the line and the char it
    /// starts at, so a click on it lands on the right column.
    #[test]
    fn row_at_reports_the_line_and_its_first_char() {
        let mut buffer = LogBuffer::new();
        buffer.push("abcdefgh", alba_executors::Stream::Stdout, false);
        buffer.push("x", alba_executors::Stream::Stdout, false);
        assert_eq!(row_at(&buffer, 5, 4, 0), Some((0, 0)));
        assert_eq!(row_at(&buffer, 5, 4, 1), Some((0, 4)));
        assert_eq!(row_at(&buffer, 5, 4, 2), Some((1, 0)));
        assert_eq!(row_at(&buffer, 5, 4, 3), None);
    }

    /// A selection across a wrap boundary is the same text as unwrapped.
    #[test]
    fn a_selection_across_a_wrap_boundary_reads_the_logical_line() {
        let mut buffer = LogBuffer::new();
        buffer.push("abcdefgh", alba_executors::Stream::Stdout, false);
        let selection = CopyState {
            anchor: (0, 2),
            cursor: (0, 5),
        };
        assert_eq!(selection.selected_text(&buffer), "cdef");
    }

    /// The window is in lines, its top the first line with a visible row.
    #[test]
    fn scroll_window_starts_at_the_first_partly_visible_line() {
        let mut buffer = LogBuffer::new();
        buffer.push("aaaaaaaa", alba_executors::Stream::Stdout, false);
        buffer.push("bb", alba_executors::Stream::Stdout, false);
        assert_eq!(scroll_window(&buffer, 2, 4), (0, 2));
        assert_eq!(top_visible_line(&buffer, 2, 4), 0);
        assert_eq!(scroll_window(&buffer, 1, 4), (1, 2));
    }
}
