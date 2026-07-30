//! The right pane: the selected beam's output, or — once the project is
//! parked — the diagnostic that parked it, with a footer naming the
//! follow state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::logs::Scroll;
use crate::state::{AppState, DIAGNOSTIC_LOG, Mode, Phase};

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState) {
    let rows = Layout::vertical([
        Constraint::Length(1), // "logs · {beam}" title
        Constraint::Min(0),    // output
        Constraint::Length(1), // follow state
    ])
    .split(area);

    // A parked session has no beam worth showing: the diagnostic that
    // parked it, filed under a pseudo-beam key, is the only thing there
    // is to read (see `state::DIAGNOSTIC_LOG`).
    let (title, key) = if matches!(state.phase, Phase::Parked) {
        ("diagnostic".to_string(), DIAGNOSTIC_LOG.to_string())
    } else {
        let name = state
            .selected_beam()
            .map(|row| row.id.clone())
            .unwrap_or_default();
        (format!("logs · {name}"), name)
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
        .map(|text| match query {
            Some(query) if !query.is_empty() => highlighted_line(&text, query),
            _ => Line::from(text),
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
}
