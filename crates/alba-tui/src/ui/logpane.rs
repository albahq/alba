//! The right pane: the selected beam's output, or — once the project is
//! parked — the diagnostic that parked it, with a footer naming the
//! follow state.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::logs::Scroll;
use crate::state::{AppState, DIAGNOSTIC_LOG, Phase};

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
    let lines: Vec<Line> = buffer
        .map(|buffer| buffer.view(body_height))
        .unwrap_or_default()
        .into_iter()
        .map(Line::from)
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
