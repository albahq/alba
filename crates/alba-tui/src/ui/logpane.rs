//! The right pane: the selected beam's output, or — once the project is
//! parked — the diagnostic that parked it, with a footer naming the
//! follow state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::copy;
use crate::logs::Scroll;
use crate::state::{AppState, Mode, Phase};

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let rows = Layout::vertical([
        Constraint::Length(1), // "logs · {beam}" title
        Constraint::Min(0),    // output
        Constraint::Length(1), // follow state
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
    let title = if matches!(state.phase, Phase::Parked) {
        "diagnostic".to_string()
    } else {
        format!("logs · {key}")
    };
    frame.render_widget(Paragraph::new(title), rows[0]);

    let buffer = state.logs.get(&key);
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
    let lines: Vec<Line> = buffer
        .map(|buffer| buffer.view(body_height))
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(row, text)| {
            // Copy mode's selection takes priority over the search
            // highlight on whichever rows it covers — the two are not
            // meant to be shown blended, and a row is never in both a
            // committed search's matches and mid-selection in a way a
            // reader needs both marked on the same line.
            if let (Mode::Copy(selection), Some(buffer)) = (&state.mode, buffer) {
                let covered = copy::line_for_pane_row(buffer, body_height, row)
                    .and_then(|index| selection.covers_line(index, text.chars().count()));
                if let Some((from, to)) = covered {
                    return copy_selected_line(&text, from, to);
                }
            }
            match query {
                Some(query) if !query.is_empty() => highlighted_line(&text, query),
                _ => Line::from(text),
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), rows[1]);

    let following = match buffer {
        Some(buffer) => matches!(buffer.scroll(), Scroll::Following),
        // No buffer yet (nothing has run) reads the same as following:
        // there is nothing to be paused partway through.
        None => true,
    };
    let footer = if following {
        "● following"
    } else {
        "↑ paused"
    };
    frame.render_widget(Paragraph::new(footer), rows[2]);
}

/// Marks every case-insensitive occurrence of `query` in `text` with a
/// reversed style, the same convention the tree pane uses for the
/// selected row. Folds case with `to_ascii_lowercase` rather than full
/// Unicode case folding: it never changes a string's byte length, so the
/// positions found in the folded copy always land on `text`'s own char
/// boundaries — a property full Unicode folding does not guarantee.
fn highlighted_line(text: &str, query: &str) -> Line<'static> {
    let haystack = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut pos = 0;
    while let Some(found) = haystack[pos..].find(&needle) {
        let start = pos + found;
        let end = start + needle.len();
        if start > pos {
            spans.push(Span::raw(text[pos..start].to_string()));
        }
        spans.push(Span::styled(
            text[start..end].to_string(),
            Style::new().reversed(),
        ));
        pos = end;
    }
    if pos < text.len() {
        spans.push(Span::raw(text[pos..].to_string()));
    }
    Line::from(spans)
}

/// Marks the inclusive character range `[from, to]` of `text` with a
/// reversed style — copy mode's selection highlight. Slices by *char*
/// index throughout, never by byte offset: a selection dragged across a
/// multi-byte character must not cut it in half.
fn copy_selected_line(text: &str, from: usize, to: usize) -> Line<'static> {
    let chars: Vec<char> = text.chars().collect();
    let from = from.min(chars.len());
    let end = (to + 1).min(chars.len());
    let mut spans = Vec::new();
    if from > 0 {
        spans.push(Span::raw(chars[..from].iter().collect::<String>()));
    }
    if end > from {
        spans.push(Span::styled(
            chars[from..end].iter().collect::<String>(),
            Style::new().reversed(),
        ));
    }
    if end < chars.len() {
        spans.push(Span::raw(chars[end..].iter().collect::<String>()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TestBackend::to_string()` drops styles, so the render snapshots
    /// cannot pin this — a unit test over the span-building function is
    /// what actually verifies a match gets marked.
    #[test]
    fn every_occurrence_of_the_query_is_marked() {
        let line = highlighted_line("Compiling api, recompiling now", "compil");
        assert_eq!(
            line,
            Line::from(vec![
                Span::styled("Compil", Style::new().reversed()),
                Span::raw("ing api, re"),
                Span::styled("compil", Style::new().reversed()),
                Span::raw("ing now"),
            ])
        );
    }

    #[test]
    fn a_line_with_no_match_is_unstyled() {
        let line = highlighted_line("warning: unused import", "error");
        assert_eq!(line, Line::from("warning: unused import"));
    }

    /// `TestBackend::to_string()` drops styles the same way it does for
    /// search — this unit test over the span-building function is what
    /// actually pins the reversed style; the snapshot in `tests/render.rs`
    /// pins layout and the bottom bar instead.
    #[test]
    fn a_copy_selection_marks_the_covered_chars() {
        let line = copy_selected_line("Compiling api", 0, 3);
        assert_eq!(
            line,
            Line::from(vec![
                Span::styled("Comp", Style::new().reversed()),
                Span::raw("iling api"),
            ])
        );
    }

    /// A selection covering the whole line has nothing before or after
    /// to leave as a plain span.
    #[test]
    fn a_full_line_selection_has_a_single_styled_span() {
        let line = copy_selected_line("bravo", 0, 4);
        assert_eq!(
            line,
            Line::from(vec![Span::styled("bravo", Style::new().reversed())])
        );
    }

    /// Marking must slice by character index, not byte offset — a
    /// multi-byte character split at the wrong boundary would panic.
    #[test]
    fn copy_selection_marking_does_not_panic_on_multibyte_characters() {
        let line = copy_selected_line("héllo wörld", 1, 3);
        assert_eq!(
            line,
            Line::from(vec![
                Span::raw("h"),
                Span::styled("éll", Style::new().reversed()),
                Span::raw("o wörld"),
            ])
        );
    }
}
