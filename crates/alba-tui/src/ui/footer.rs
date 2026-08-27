//! The footer row: the run, and nothing else. Left, the counts by
//! status in the tree's own colours; right, the progress bar with
//! `done/total` and the elapsed time while a run is going, the outcome
//! (`ok`/`failed` and the duration) once it is over, `idle` before any
//! run. It sits under the junction line that closes the two panes and
//! above the bottom edge's keymap (see `ui/mod.rs`).
//!
//! The counts tally only the four statuses a beam can settle into
//! (`✔ ⚡ ✖ ○`) and leave a beam still `▶` running out: it has not
//! settled into an outcome yet, so counting it under any of the four
//! would misreport which bucket it will land in.

use std::time::{Duration, Instant};

use alba_engine::{BeamStatus, RunSummary};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::state::{AppState, BeamState, Phase};

use super::format_duration;
use super::theme::{bar_style, outcome_style, status_style};

/// The bar's widest. It has only `total + 1` distinct states, so more
/// cells would make each step jump further without saying anything new.
const BAR_MAX: usize = 30;
/// Below this, the bar is dropped rather than squeezed: the spec's
/// original mockup drew 14 cells, and fewer stop reading as a bar.
const BAR_MIN: usize = 14;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, now: Instant) {
    // One cell of margin at each edge, the same as the tree's rows.
    let row = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let counts = counts_line(state);
    // What is left for the run's text once the counts and a two-cell
    // gap are placed. A vanished status bucket misreports how many
    // beams actually landed in it, which is worse than the run's own
    // text reading incomplete, so the counts win: the run's paragraph
    // is confined to this room instead of the whole row, and is
    // visibly clipped rather than drawn over the counts' own cells.
    let room = (row.width as usize).saturating_sub(counts.width() + 2);
    let run = run_line(state, now, room);
    let run_area = Rect {
        x: row.x + (row.width as usize).saturating_sub(room) as u16,
        width: room as u16,
        ..row
    };
    // Two paragraphs on the one row: neither clears it, so the second
    // does not paint over the first.
    frame.render_widget(Paragraph::new(counts), row);
    frame.render_widget(Paragraph::new(run).alignment(Alignment::Right), run_area);
}

/// The run's own text, fitted into `room` cells: the bar only when it
/// has at least `BAR_MIN` cells to itself after the count and the time.
fn run_line(state: &AppState, now: Instant, room: usize) -> Line<'static> {
    match &state.phase {
        Phase::Running { done, total, since } => {
            let elapsed = now.saturating_duration_since(*since);
            let text = format!("{done}/{total} · {}", format_duration(elapsed));
            match bar_width(room.saturating_sub(text.width() + 1)) {
                Some(width) => {
                    let filled = filled_cells(*done, *total, width);
                    Line::from(vec![
                        Span::styled("▰".repeat(filled), bar_style(state.colour)),
                        Span::raw(format!("{} {text}", "▱".repeat(width - filled))),
                    ])
                }
                None => Line::from(text),
            }
        }
        // A watch waiting between runs, and a parked project, still
        // show the last run's outcome: the session state has its own
        // slot on the top edge, so nothing here has to give way to it.
        Phase::Finished | Phase::Waiting { .. } | Phase::Parked => match &state.last_summary {
            Some(summary) => outcome_line(summary, state.colour),
            None => Line::from("idle"),
        },
    }
}

/// The bar's width for `room` free cells: `None` under `BAR_MIN`,
/// capped at `BAR_MAX`.
fn bar_width(room: usize) -> Option<usize> {
    (room >= BAR_MIN).then(|| room.min(BAR_MAX))
}

/// Deterministic on purpose: `done`/`total` are the whole story, so the
/// same state always draws the same bar and no clock is involved in
/// deciding how full it looks — only in how long the run has taken,
/// which `run_line` prints separately.
fn filled_cells(done: usize, total: usize, width: usize) -> usize {
    if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * width as f64).round() as usize
    }
    .min(width)
}

/// The last run's outcome, one word in the outcome colour and the
/// duration. An allowed failure reads `failed`, as `outcome_style` and
/// the tree's `✖` both already treat it.
fn outcome_line(summary: &RunSummary, colour: bool) -> Line<'static> {
    let ran = summary.succeeded.len()
        + summary.cached.len()
        + summary.failed.len()
        + summary.failed_allowed.len()
        + summary.cancelled.len();
    if ran == 0 {
        return Line::from("nothing ran");
    }
    let word = if summary.failed.is_empty() && summary.failed_allowed.is_empty() {
        "ok"
    } else {
        "failed"
    };
    Line::from(vec![
        Span::styled(word.to_string(), outcome_style(summary, colour)),
        Span::raw(format!(" · {}", format_duration(summary.duration))),
    ])
}

/// The four buckets a beam can settle into, each `glyph count` pair
/// styled by `status_style` of a representative state for that bucket:
/// the same colour the tree's own rows would show that status in.
fn counts_line(state: &AppState) -> Line<'static> {
    let mut succeeded = 0;
    let mut cached = 0;
    let mut failed = 0;
    let mut pending_or_cancelled = 0;
    for row in &state.beams {
        match &row.state {
            BeamState::Pending => pending_or_cancelled += 1,
            BeamState::Running { .. } => {}
            BeamState::Done { status, .. } => match status {
                BeamStatus::Succeeded => succeeded += 1,
                BeamStatus::Cached => cached += 1,
                BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => failed += 1,
                BeamStatus::Cancelled => pending_or_cancelled += 1,
            },
        }
    }
    let duration = Duration::from_secs(0);
    let buckets: [(&str, usize, BeamState); 4] = [
        (
            "✔",
            succeeded,
            BeamState::Done {
                status: BeamStatus::Succeeded,
                duration,
            },
        ),
        (
            "⚡",
            cached,
            BeamState::Done {
                status: BeamStatus::Cached,
                duration,
            },
        ),
        (
            "✖",
            failed,
            BeamState::Done {
                status: BeamStatus::Failed { exit_code: 1 },
                duration,
            },
        ),
        ("○", pending_or_cancelled, BeamState::Pending),
    ];
    let mut spans = Vec::with_capacity(buckets.len() * 2 - 1);
    for (index, (glyph, count, representative)) in buckets.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("{glyph} {count}"),
            status_style(representative, state.colour),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use ratatui::style::{Color, Style};

    fn state_with_phase(phase: Phase) -> AppState {
        let mut state = AppState::new("build", false);
        state.phase = phase;
        state
    }

    fn running(done: usize, total: usize, now: Instant) -> AppState {
        state_with_phase(Phase::Running {
            done,
            total,
            since: now - Duration::from_secs(3),
        })
    }

    #[test]
    fn the_bar_needs_fourteen_cells_and_stops_at_thirty() {
        assert_eq!(bar_width(13), None);
        assert_eq!(bar_width(14), Some(14));
        assert_eq!(bar_width(20), Some(20));
        assert_eq!(bar_width(55), Some(30));
    }

    /// The spec's own worked example: 2/5 done draws 6 of 14 cells filled.
    #[test]
    fn filled_cells_rounds_to_the_nearest_cell() {
        assert_eq!(filled_cells(2, 5, 14), 6);
        assert_eq!(filled_cells(0, 5, 14), 0);
        assert_eq!(filled_cells(5, 5, 14), 14);
        assert_eq!(filled_cells(0, 0, 14), 0);
    }

    #[test]
    fn a_run_in_flight_draws_the_bar_the_count_and_the_time() {
        let now = Instant::now();
        // 55 cells of room: "2/5 · 3.0s" (10) plus a space leaves 44,
        // capped at 30.
        assert_eq!(
            run_line(&running(2, 5, now), now, 55).to_string(),
            format!("{}{} 2/5 · 3.0s", "▰".repeat(12), "▱".repeat(18))
        );
    }

    #[test]
    fn a_narrow_footer_drops_the_bar_and_keeps_the_count() {
        let now = Instant::now();
        // 20 cells of room: 10 for the text, a space, 9 left: under 14.
        assert_eq!(
            run_line(&running(2, 5, now), now, 20).to_string(),
            "2/5 · 3.0s"
        );
    }

    #[test]
    fn a_finished_run_reads_ok_or_failed_with_its_duration() {
        let mut state = state_with_phase(Phase::Finished);
        state.last_summary = Some(RunSummary {
            succeeded: vec![BeamId("a".into())],
            duration: Duration::from_millis(500),
            ..RunSummary::default()
        });
        assert_eq!(
            run_line(&state, Instant::now(), 55).to_string(),
            "ok · 0.5s"
        );

        state.last_summary = Some(RunSummary {
            failed_allowed: vec![BeamId("a".into())],
            duration: Duration::from_secs(2),
            ..RunSummary::default()
        });
        assert_eq!(
            run_line(&state, Instant::now(), 55).to_string(),
            "failed · 2.0s"
        );
    }

    #[test]
    fn an_empty_summary_reads_nothing_ran_and_no_summary_reads_idle() {
        let mut state = state_with_phase(Phase::Finished);
        assert_eq!(run_line(&state, Instant::now(), 55).to_string(), "idle");
        state.last_summary = Some(RunSummary::default());
        assert_eq!(
            run_line(&state, Instant::now(), 55).to_string(),
            "nothing ran"
        );
    }

    /// A watch waiting between runs keeps the outcome on the footer.
    #[test]
    fn a_waiting_watch_still_shows_the_last_outcome() {
        let mut state = state_with_phase(Phase::Waiting { files: 3 });
        state.last_summary = Some(RunSummary {
            failed: vec![BeamId("a".into())],
            duration: Duration::from_secs(1),
            ..RunSummary::default()
        });
        assert_eq!(
            run_line(&state, Instant::now(), 55).to_string(),
            "failed · 1.0s"
        );
    }

    /// `TestBackend::to_string()` (the render snapshots) drops styles, so
    /// this is what proves the bar and the outcome reach their colour
    /// functions with `state.colour`.
    #[test]
    fn colour_reaches_the_bar_and_the_outcome() {
        let now = Instant::now();
        let mut state = running(1, 2, now);
        state.colour = true;
        assert_eq!(
            run_line(&state, now, 55).spans[0].style,
            Style::new().fg(Color::Green)
        );

        let mut finished = state_with_phase(Phase::Finished);
        finished.colour = true;
        finished.last_summary = Some(RunSummary {
            failed: vec![BeamId("a".into())],
            ..RunSummary::default()
        });
        assert_eq!(
            run_line(&finished, now, 55).spans[0].style,
            Style::new().fg(Color::Red)
        );
    }

    /// Wide counts (many beams) must never lose a bucket to the run's
    /// own text: at 40 columns, 100 beams in each of the four buckets
    /// leaves no room for `failed · 12.0s` beside them, so the run's
    /// text is the one that gives way, clipped rather than drawn over
    /// the counts.
    #[test]
    fn wide_counts_are_never_overwritten_by_the_run_text() {
        use crate::state::BeamRow;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = state_with_phase(Phase::Finished);
        state.last_summary = Some(RunSummary {
            failed: vec![BeamId("a".into())],
            duration: Duration::from_secs(12),
            ..RunSummary::default()
        });
        let mut beams = Vec::new();
        for bucket in 0..4 {
            let status = match bucket {
                0 => BeamState::Done {
                    status: BeamStatus::Succeeded,
                    duration: Duration::from_secs(0),
                },
                1 => BeamState::Done {
                    status: BeamStatus::Cached,
                    duration: Duration::from_secs(0),
                },
                2 => BeamState::Done {
                    status: BeamStatus::Failed { exit_code: 1 },
                    duration: Duration::from_secs(0),
                },
                _ => BeamState::Pending,
            };
            for index in 0..100 {
                beams.push(BeamRow {
                    id: format!("beam-{bucket}-{index}"),
                    state: status.clone(),
                });
            }
        }
        state.beams = beams;

        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &state, Instant::now()))
            .unwrap();
        let row = terminal.backend().to_string();
        assert!(
            row.contains("✔ 100")
                && row.contains("⚡ 100")
                && row.contains("✖ 100")
                && row.contains("○ 100"),
            "every bucket survives the run's own text: {row}"
        );
    }
}
