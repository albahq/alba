//! Incremental search over one beam's log buffer.
//!
//! This module is pure: no I/O, no terminal, no clock. It knows the
//! query and the line indices it matches, nothing about how those
//! indices get drawn or scrolled to — that is `ui/logpane.rs`'s job, and
//! the `AppState` methods that carry the buffer's `Paused` offset along
//! with the search. Matching folds case with `to_ascii_lowercase`
//! rather than full Unicode case folding: build output is overwhelmingly
//! ASCII, and byte-length-preserving folding is also what lets
//! `ui/logpane.rs` slice the original text at match boundaries without
//! ever landing off a char boundary.

use crate::logs::LogBuffer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    /// Line indices (into the buffer, oldest first) that match `query`,
    /// recomputed by `update` rather than maintained incrementally: a
    /// single beam's buffer is small enough that a full rescan per
    /// keystroke is simpler than tracking edits.
    pub matches: Vec<usize>,
    /// Index into `matches`, not a line number.
    pub current: usize,
}

impl SearchState {
    pub fn new() -> Self {
        Self {
            query: String::new(),
            matches: Vec::new(),
            current: 0,
        }
    }

    /// Recomputes `matches` (line indices, case-insensitive substring)
    /// against `buffer`. Callers run this after every keystroke and
    /// whenever the buffer gains lines — `SearchState` itself samples
    /// neither the clock nor the buffer on its own. An empty query
    /// matches nothing rather than every line: `"".contains("")` is
    /// true, which would otherwise light up the whole pane.
    pub fn update(&mut self, buffer: &LogBuffer) {
        self.matches.clear();
        if !self.query.is_empty() {
            let needle = self.query.to_ascii_lowercase();
            self.matches.extend(
                buffer
                    .lines()
                    .enumerate()
                    .filter(|(_, line)| line.text.to_ascii_lowercase().contains(&needle))
                    .map(|(index, _)| index),
            );
        }
        self.current = 0;
    }

    pub fn push_char(&mut self, character: char) {
        self.query.push(character);
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
    }

    /// `n`: the next match, wrapping past the last back to the first.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    /// `N`: the previous match, wrapping past the first back to the last.
    pub fn previous(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// The line the pane should scroll to, if any.
    pub fn current_line(&self) -> Option<usize> {
        self.matches.get(self.current).copied()
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer_of(lines: &[&str]) -> LogBuffer {
        let mut buffer = LogBuffer::new();
        for text in lines {
            buffer.push(text.to_string(), false);
        }
        buffer
    }

    fn typed(query: &str) -> SearchState {
        let mut search = SearchState::new();
        for character in query.chars() {
            search.push_char(character);
        }
        search
    }

    #[test]
    fn matches_are_case_insensitive_and_incremental() {
        let buffer = buffer_of(&["Compiling api", "warning: unused", "Compiling core"]);
        let mut search = typed("compiling");
        search.update(&buffer);
        assert_eq!(search.matches, vec![0, 2]);
        assert_eq!(search.current_line(), Some(0));
        search.next();
        assert_eq!(search.current_line(), Some(2));
        search.next(); // wraps
        assert_eq!(search.current_line(), Some(0));
        search.previous();
        assert_eq!(search.current_line(), Some(2));
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let buffer = buffer_of(&["anything"]);
        let mut search = SearchState::new();
        search.update(&buffer);
        assert_eq!(search.current_line(), None);
    }

    /// Backspace shrinks the query; recomputing then narrows the matches
    /// rather than leaving the wider set from the longer query behind.
    #[test]
    fn pop_char_shrinks_the_query_and_the_matches_narrow_on_update() {
        let buffer = buffer_of(&["Compiling api", "warning: unused"]);
        let mut search = typed("Compilingx");
        search.update(&buffer);
        assert_eq!(
            search.matches,
            Vec::<usize>::new(),
            "no line contains 'Compilingx'"
        );
        search.pop_char();
        assert_eq!(search.query, "Compiling");
        search.update(&buffer);
        assert_eq!(search.matches, vec![0]);
    }

    /// `update` is called again whenever the buffer gains lines — a new
    /// line matching the live query must show up without retyping it.
    #[test]
    fn update_picks_up_lines_the_buffer_gained_since() {
        let mut buffer = buffer_of(&["Compiling api"]);
        let mut search = typed("compiling");
        search.update(&buffer);
        assert_eq!(search.matches, vec![0]);
        buffer.push("Compiling core".to_string(), false);
        search.update(&buffer);
        assert_eq!(search.matches, vec![0, 1]);
    }

    /// A single match neither advances nor regresses past itself.
    #[test]
    fn a_single_match_wraps_to_itself() {
        let buffer = buffer_of(&["Compiling api"]);
        let mut search = typed("compiling");
        search.update(&buffer);
        search.next();
        assert_eq!(search.current_line(), Some(0));
        search.previous();
        assert_eq!(search.current_line(), Some(0));
    }

    /// Stepping with no matches at all must not panic.
    #[test]
    fn stepping_with_no_matches_is_a_no_op() {
        let buffer = buffer_of(&["anything"]);
        let mut search = typed("nope");
        search.update(&buffer);
        search.next();
        search.previous();
        assert_eq!(search.current_line(), None);
    }
}
