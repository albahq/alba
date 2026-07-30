//! Per-beam log storage: a bounded ring buffer with a following viewport.
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

/// The memory bound a day-long watch session relies on.
pub const MAX_LINES: usize = 10_000;

pub struct LogLine {
    pub text: String,
    pub replayed: bool,
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

    /// While paused, an append must not move the pinned view: growing the
    /// tail by one pushes the pin's offset out by one to compensate. When
    /// the append also drops the oldest line (past the cap), that drop
    /// shrinks the reachable top by one, cancelling the compensation —
    /// which is exactly the clamp a paused-at-the-top reader needs: the
    /// offset tracks whatever is currently oldest instead of dangling
    /// past it.
    pub fn push(&mut self, text: String, replayed: bool) {
        self.lines.push_back(LogLine { text, replayed });
        if let Scroll::Paused { offset } = &mut self.scroll {
            *offset += 1;
        }
        if self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.truncated += 1;
            if let Scroll::Paused { offset } = &mut self.scroll {
                *offset = offset.saturating_sub(1);
            }
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
    pub fn view(&self, height: usize) -> Vec<String> {
        let len = self.lines.len();
        let offset = match self.scroll {
            Scroll::Following => 0,
            Scroll::Paused { offset } => offset,
        };
        let end = len.saturating_sub(offset);
        let start = end.saturating_sub(height);

        if start == 0 && self.truncated > 0 {
            let marker = format!("… {} older lines truncated", self.truncated);
            let real_count = height.saturating_sub(1).min(end);
            std::iter::once(marker)
                .chain(
                    self.lines
                        .iter()
                        .take(real_count)
                        .map(|line| line.text.clone()),
                )
                .collect()
        } else {
            self.lines
                .iter()
                .skip(start)
                .take(end - start)
                .map(|line| line.text.clone())
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

    fn filled(count: usize) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        for index in 0..count {
            buffer.push(format!("line {index}"), false);
        }
        buffer
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
        assert_eq!(view[0], "… 5 older lines truncated");
        assert_eq!(view[1], "line 5");
    }

    /// Following by default: the view ends at the tail.
    #[test]
    fn a_following_buffer_shows_the_tail() {
        let buffer = filled(100);
        let view = buffer.view(3);
        assert_eq!(view, vec!["line 97", "line 98", "line 99"]);
    }

    /// Scrolling up suspends following; new lines no longer move the view.
    #[test]
    fn scrolling_up_pauses_and_pins_the_view() {
        let mut buffer = filled(100);
        buffer.scroll_up(10);
        assert!(matches!(buffer.scroll(), Scroll::Paused { offset: 10 }));
        let pinned = buffer.view(3);
        buffer.push("line 100".to_string(), false);
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
            buffer.push(format!("extra {index}"), false);
        }
        let view = buffer.view(2);
        assert_eq!(view[0], "… 10 older lines truncated");
    }
}
