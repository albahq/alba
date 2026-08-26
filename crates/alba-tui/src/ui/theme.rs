//! The one place a status becomes a colour. Sixteen ANSI colours only,
//! and every function answers `Style::default()` when `colour` is off,
//! so `NO_COLOR` renders exactly the monochrome interface it always did.

use alba_engine::{BeamStatus, RunSummary};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};

use crate::state::BeamState;

pub fn status_style(state: &BeamState, colour: bool) -> Style {
    if !colour {
        return Style::default();
    }
    match state {
        BeamState::Pending => Style::new().dim(),
        BeamState::Running { .. } => Style::new().fg(Color::Yellow),
        BeamState::Done { status, .. } => match status {
            BeamStatus::Succeeded => Style::new().fg(Color::Green),
            BeamStatus::Cached => Style::new().fg(Color::Cyan),
            BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => {
                Style::new().fg(Color::Red)
            }
            BeamStatus::Cancelled => Style::new().dim(),
        },
    }
}

/// Green when no beam failed, red otherwise; an allowed failure is a
/// failure here, as it is in the tree's `✖`.
pub fn outcome_style(summary: &RunSummary, colour: bool) -> Style {
    if !colour {
        return Style::default();
    }
    if summary.failed.is_empty() && summary.failed_allowed.is_empty() {
        Style::new().fg(Color::Green)
    } else {
        Style::new().fg(Color::Red)
    }
}

pub fn parked_style(colour: bool) -> Style {
    if colour {
        Style::new().fg(Color::Yellow)
    } else {
        Style::default()
    }
}

/// The filled part of the progress bar.
pub fn bar_style(colour: bool) -> Style {
    if colour {
        Style::new().fg(Color::Green)
    } else {
        Style::default()
    }
}

pub fn key_style(colour: bool) -> Style {
    if colour {
        Style::new().bold()
    } else {
        Style::default()
    }
}

/// A bottom-bar text (`q quit · r rerun`) as a line whose first word of
/// every ` · `-separated item is a key drawn with `key_style`. Off
/// colour it is one plain span, the text unchanged.
///
/// "First word of the item" is a heuristic for "the key", not a rule
/// `bottom_bar`'s callers are held to, and it has two known misfires
/// left as cosmetic: an item with no space at all bolds whole, not just
/// a first word (the Search bar's `1/5` match counter), and an item
/// whose first word is not the key (`copied (OSC 52) · q quit` bolds
/// `copied`) bolds the wrong word. Neither is worth restructuring
/// `bottom_bar` to fix.
pub fn bar_line(text: &str, colour: bool) -> Line<'static> {
    if !colour {
        return Line::from(text.to_string());
    }
    let mut spans = Vec::new();
    for (index, item) in text.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::raw(" · "));
        }
        match item.split_once(' ') {
            Some((key, label)) => {
                spans.push(Span::styled(key.to_string(), key_style(colour)));
                spans.push(Span::raw(format!(" {label}")));
            }
            None => spans.push(Span::styled(item.to_string(), key_style(colour))),
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use std::time::{Duration, Instant};

    fn done(status: BeamStatus) -> BeamState {
        BeamState::Done {
            status,
            duration: Duration::from_secs(1),
        }
    }

    #[test]
    fn each_status_has_its_colour() {
        assert_eq!(
            status_style(&done(BeamStatus::Succeeded), true),
            Style::new().fg(Color::Green)
        );
        assert_eq!(
            status_style(&done(BeamStatus::Cached), true),
            Style::new().fg(Color::Cyan)
        );
        assert_eq!(
            status_style(
                &BeamState::Running {
                    since: Instant::now()
                },
                true
            ),
            Style::new().fg(Color::Yellow)
        );
        assert_eq!(
            status_style(&done(BeamStatus::Failed { exit_code: 1 }), true),
            Style::new().fg(Color::Red)
        );
        assert_eq!(
            status_style(&done(BeamStatus::FailedAllowed { exit_code: 1 }), true),
            Style::new().fg(Color::Red)
        );
        assert_eq!(status_style(&BeamState::Pending, true), Style::new().dim());
        assert_eq!(
            status_style(&done(BeamStatus::Cancelled), true),
            Style::new().dim()
        );
    }

    #[test]
    fn colour_off_is_the_default_style_everywhere() {
        assert_eq!(
            status_style(&done(BeamStatus::Failed { exit_code: 1 }), false),
            Style::default()
        );
        assert_eq!(status_style(&BeamState::Pending, false), Style::default());
        assert_eq!(
            outcome_style(&RunSummary::default(), false),
            Style::default()
        );
        assert_eq!(key_style(false), Style::default());
        assert_eq!(bar_style(false), Style::default());
        assert_eq!(parked_style(false), Style::default());
    }

    #[test]
    fn the_outcome_is_red_on_any_failure_and_green_otherwise() {
        let green = RunSummary {
            succeeded: vec![BeamId("a".into())],
            ..RunSummary::default()
        };
        assert_eq!(outcome_style(&green, true), Style::new().fg(Color::Green));
        let allowed = RunSummary {
            failed_allowed: vec![BeamId("a".into())],
            ..RunSummary::default()
        };
        assert_eq!(outcome_style(&allowed, true), Style::new().fg(Color::Red));
        let failed = RunSummary {
            failed: vec![BeamId("a".into())],
            ..RunSummary::default()
        };
        assert_eq!(outcome_style(&failed, true), Style::new().fg(Color::Red));
    }

    /// `q quit · r rerun` becomes bold keys and plain labels, separated
    /// by the same ` · ` the plain text had.
    #[test]
    fn bar_line_bolds_the_keys() {
        let line = bar_line("q quit · Esc cancel", true);
        let spans: Vec<(String, Style)> = line
            .spans
            .iter()
            .map(|span| (span.content.to_string(), span.style))
            .collect();
        assert_eq!(
            spans,
            vec![
                ("q".to_string(), Style::new().bold()),
                (" quit".to_string(), Style::default()),
                (" · ".to_string(), Style::default()),
                ("Esc".to_string(), Style::new().bold()),
                (" cancel".to_string(), Style::default()),
            ]
        );
        assert_eq!(bar_line("q quit", false).spans.len(), 1);
    }
}
