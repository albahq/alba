//! The progress row, under the header: a bar on the left, the counts
//! on the right. In a watch session between runs the banner row takes
//! its place.
//!
//! The counts read `done/total complete`, then each non-zero bucket
//! with its glyph. A beam still `▶` running is in none of them: it has
//! not settled yet. While a run is going the `○` bucket counts the beams
//! not started (`pending`); once it is over, every beam that did not run
//! (`skipped`).

use alba_engine::BeamStatus;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::state::{AppState, BeamState, Phase};

use super::beams::fit_name;
use super::format_duration;
use super::theme::{ColourMode, Token, style};

/// Below this, the bar is dropped rather than squeezed: fewer cells stop
/// reading as a bar.
const BAR_MIN: usize = 14;
/// Between the bar and the counts.
const BAR_GAP: usize = 2;
/// Between two count groups, and between the banner's two halves at
/// their closest.
const GROUP_GAP: &str = "   ";
const BANNER_GAP: usize = 2;

#[derive(Default)]
struct Counts {
    done: usize,
    total: usize,
    passed: usize,
    cached: usize,
    failed: usize,
    not_run: usize,
}

impl Counts {
    fn of(state: &AppState) -> Self {
        let running = state.running();
        let mut counts = Counts {
            total: state.beams.len(),
            ..Counts::default()
        };
        for row in &state.beams {
            match &row.state {
                BeamState::Pending => counts.not_run += 1,
                BeamState::Running { .. } => {}
                BeamState::Done { status, .. } => match status {
                    BeamStatus::Succeeded => {
                        counts.done += 1;
                        counts.passed += 1;
                    }
                    BeamStatus::Cached => {
                        counts.done += 1;
                        counts.cached += 1;
                    }
                    BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => {
                        counts.done += 1;
                        counts.failed += 1;
                    }
                    // A cancelled beam, skipped or cut short, is not
                    // complete: pending no more while the run goes on,
                    // and skipped once it is over.
                    BeamStatus::Cancelled if !running => counts.not_run += 1,
                    BeamStatus::Cancelled => {}
                },
            }
        }
        counts
    }
}

/// Which group a count belongs to, in the order they are drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Complete,
    Passed,
    Cached,
    Failed,
    NotRun,
}

/// The progress row for a row `width` cells wide.
pub fn line(state: &AppState, width: u16) -> Line<'static> {
    let mode = state.colour;
    let width = width as usize;
    let counts = Counts::of(state);
    let mut groups = vec![Group::Complete];
    for (group, count) in [
        (Group::Passed, counts.passed),
        (Group::Cached, counts.cached),
        (Group::Failed, counts.failed),
        (Group::NotRun, counts.not_run),
    ] {
        if count > 0 {
            groups.push(group);
        }
    }
    let mut complete_word = true;
    let not_run_label = if state.running() {
        "pending"
    } else {
        "skipped"
    };
    let render = |groups: &[Group], complete_word: bool| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (index, group) in groups.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(GROUP_GAP));
            }
            let (glyph, token, count, label) = match group {
                Group::Complete => {
                    spans.push(Span::styled(
                        format!("{}/{}", counts.done, counts.total),
                        style(Token::Text, mode),
                    ));
                    if complete_word {
                        spans.push(Span::styled(" complete", style(Token::TextMuted, mode)));
                    }
                    continue;
                }
                Group::Passed => ("✓", Token::Success, counts.passed, "passed"),
                Group::Cached => ("↯", Token::Cached, counts.cached, "cached"),
                Group::Failed => ("✗", Token::Error, counts.failed, "failed"),
                Group::NotRun => ("○", Token::TextMuted, counts.not_run, not_run_label),
            };
            spans.push(Span::styled(glyph, style(token, mode)));
            spans.push(Span::styled(
                format!(" {count} {label}"),
                style(Token::Text, mode),
            ));
        }
        spans
    };
    let spans_width = |spans: &[Span]| spans.iter().map(|span| span.width()).sum::<usize>();

    // Groups give way in a fixed order; `done/total complete` and the
    // failures never do, and past them only the word `complete` goes.
    let mut counts_spans = render(&groups, complete_word);
    for dropped in [Group::NotRun, Group::Cached, Group::Passed] {
        if spans_width(&counts_spans) <= width {
            break;
        }
        groups.retain(|group| *group != dropped);
        counts_spans = render(&groups, complete_word);
    }
    if spans_width(&counts_spans) > width {
        complete_word = false;
        counts_spans = render(&groups, complete_word);
    }

    let counts_width = spans_width(&counts_spans);
    let room = width.saturating_sub(counts_width + BAR_GAP);
    let mut spans = if room >= BAR_MIN {
        bar(&counts, room, mode)
    } else {
        vec![Span::raw(" ".repeat(width.saturating_sub(counts_width)))]
    };
    if room >= BAR_MIN {
        spans.push(Span::raw(" ".repeat(BAR_GAP)));
    }
    spans.extend(counts_spans);
    Line::from(spans)
}

/// `width` cells of `━` for the done part and `─` for the rest. The fill
/// turns `error` once any beam has failed.
fn bar(counts: &Counts, width: usize, mode: ColourMode) -> Vec<Span<'static>> {
    let filled = if counts.total == 0 {
        0
    } else {
        ((counts.done as f64 / counts.total as f64) * width as f64).round() as usize
    }
    .min(width);
    let fill = if counts.failed > 0 {
        Token::Error
    } else {
        Token::Success
    };
    vec![
        Span::styled("━".repeat(filled), style(fill, mode)),
        Span::styled("─".repeat(width - filled), style(Token::Track, mode)),
    ]
}

/// The banner that replaces the progress row while a watch session
/// waits between runs or is parked; `None` otherwise. The left half in
/// `text` bold, the right half in `text-muted`; when narrow the right
/// half goes first, then the left is cut with `…`.
pub fn banner(state: &AppState, width: u16) -> Option<Line<'static>> {
    let mode = state.colour;
    let width = width as usize;
    let (left, right) = match &state.phase {
        Phase::Waiting { .. } => {
            let left = state.last_summary.as_ref().map(|summary| {
                let time = format_duration(summary.duration);
                if state.run_cancelled() {
                    format!("Last run cancelled after {time}.")
                } else if summary.failed.is_empty() && summary.failed_allowed.is_empty() {
                    format!("Last run passed in {time}.")
                } else {
                    format!("Last run failed in {time}.")
                }
            });
            (
                left.unwrap_or_default(),
                "Save a watched file to run again.",
            )
        }
        Phase::Parked => (
            "Invalid Beamfile.".to_string(),
            "Fix the error and save to resume.",
        ),
        Phase::Running { .. } | Phase::Finished => return None,
    };
    let gap = if left.is_empty() { 0 } else { BANNER_GAP };
    let mut spans = Vec::new();
    if left.width() + gap + right.width() <= width {
        let pad = width - left.width() - right.width();
        spans.push(Span::styled(left, style(Token::Text, mode).bold()));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(right, style(Token::TextMuted, mode)));
    } else {
        spans.push(Span::styled(
            fit_name(&left, width),
            style(Token::Text, mode).bold(),
        ));
    }
    Some(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::{RunEvent, RunSummary};
    use ratatui::style::Color;
    use std::time::{Duration, Instant};

    fn id(name: &str) -> BeamId {
        BeamId(name.to_string())
    }

    fn started(beams: &[&str]) -> AppState {
        let mut state = AppState::new("build", true);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id("build")],
                affected_by: None,
                beams: beams.iter().map(|name| id(name)).collect(),
                edges: Vec::new(),
                details: Vec::new(),
            },
            Instant::now(),
        );
        state
    }

    fn finish(state: &mut AppState, beam: &str, status: BeamStatus) {
        if !matches!(status, BeamStatus::Cancelled) {
            state.apply(&RunEvent::BeamStarted { id: id(beam) }, Instant::now());
        }
        state.apply(
            &RunEvent::BeamFinished {
                id: id(beam),
                status,
                duration: Duration::from_millis(100),
            },
            Instant::now(),
        );
    }

    /// Six beams: passed, cached, failed, then three pending.
    fn mixed() -> AppState {
        let mut state = started(&["a", "b", "c", "d", "e", "f"]);
        finish(&mut state, "a", BeamStatus::Succeeded);
        finish(&mut state, "b", BeamStatus::Cached);
        finish(&mut state, "c", BeamStatus::Failed { exit_code: 1 });
        state
    }

    fn drawn(state: &AppState, width: u16) -> String {
        let line = line(state, width);
        assert!(line.width() <= width as usize, "{line} overflows {width}");
        line.to_string()
    }

    #[test]
    fn progress_counts_drop_in_order_when_narrow() {
        let state = mixed();
        let full = "3/6 complete   ✓ 1 passed   ↯ 1 cached   ✗ 1 failed   ○ 3 pending";
        assert_eq!(full.width(), 65);
        assert_eq!(drawn(&state, 65), full);
        assert_eq!(
            drawn(&state, 64),
            format!(
                "{}3/6 complete   ✓ 1 passed   ↯ 1 cached   ✗ 1 failed",
                " ".repeat(13)
            )
        );
        assert_eq!(
            drawn(&state, 50).trim_start(),
            "3/6 complete   ✓ 1 passed   ✗ 1 failed"
        );
        assert_eq!(drawn(&state, 30).trim_start(), "3/6 complete   ✗ 1 failed");
        assert_eq!(drawn(&state, 20).trim_start(), "3/6   ✗ 1 failed");
    }

    #[test]
    fn the_bar_takes_the_room_left_and_vanishes_under_fourteen_cells() {
        let state = mixed();
        let counts = "3/6 complete   ✓ 1 passed   ↯ 1 cached   ✗ 1 failed   ○ 3 pending";
        // 65 for the counts, 2 of gap, 20 of bar: 10 filled.
        assert_eq!(
            drawn(&state, 87),
            format!("{}{}  {counts}", "━".repeat(10), "─".repeat(10))
        );
        assert_eq!(drawn(&state, 80), format!("{}{counts}", " ".repeat(15)));
    }

    #[test]
    fn the_progress_fill_turns_red_after_a_failure() {
        let mut state = started(&["a", "b"]).with_colour(ColourMode::TrueColour);
        finish(&mut state, "a", BeamStatus::Succeeded);
        let green = line(&state, 80);
        assert_eq!(green.spans[0].style.fg, Some(Color::Rgb(0xA5, 0xCF, 0x9A)));
        assert_eq!(green.spans[1].style.fg, Some(Color::Rgb(0x79, 0x80, 0x7C)));

        finish(&mut state, "b", BeamStatus::FailedAllowed { exit_code: 1 });
        let red = line(&state, 80);
        assert_eq!(red.spans[0].style.fg, Some(Color::Rgb(0xF0, 0xA2, 0x9A)));
    }

    /// A cancelled beam is not complete: it counts as skipped once the
    /// run is over, never towards `done`.
    #[test]
    fn a_finished_run_counts_skipped_beams() {
        let mut state = started(&["a", "b", "c"]);
        finish(&mut state, "a", BeamStatus::Failed { exit_code: 1 });
        finish(&mut state, "b", BeamStatus::Cancelled);
        assert_eq!(
            drawn(&state, 40).trim_start(),
            "1/3 complete   ✗ 1 failed   ○ 1 pending"
        );
        state.apply(
            &RunEvent::RunFinished {
                summary: RunSummary::default(),
            },
            Instant::now(),
        );
        assert_eq!(
            drawn(&state, 40).trim_start(),
            "1/3 complete   ✗ 1 failed   ○ 2 skipped"
        );
    }

    #[test]
    fn the_banner_replaces_progress_between_watch_runs() {
        let mut state = started(&["a"]);
        assert!(banner(&state, 80).is_none());
        finish(&mut state, "a", BeamStatus::Succeeded);
        state.apply(
            &RunEvent::RunFinished {
                summary: RunSummary {
                    succeeded: vec![id("a")],
                    duration: Duration::from_millis(9400),
                    ..RunSummary::default()
                },
            },
            Instant::now(),
        );
        assert!(banner(&state, 80).is_none());
        state.apply(&RunEvent::WatchWaiting { files: 3 }, Instant::now());
        let left = "Last run passed in 9.4s.";
        let right = "Save a watched file to run again.";
        assert_eq!(
            banner(&state, 80).unwrap().to_string(),
            format!("{left}{}{right}", " ".repeat(80 - left.len() - right.len()))
        );
        assert_eq!(banner(&state, 40).unwrap().to_string(), left);

        state.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\n".to_string(),
                report: None,
            },
            Instant::now(),
        );
        let parked = banner(&state, 60).unwrap().to_string();
        assert!(parked.starts_with("Invalid Beamfile."), "{parked}");
        assert!(
            parked.ends_with("Fix the error and save to resume."),
            "{parked}"
        );
        assert_eq!(banner(&state, 10).unwrap().to_string(), "Invalid B…");
    }

    #[test]
    fn a_failed_watch_run_says_so_in_the_banner() {
        let mut state = started(&["a"]);
        state.apply(
            &RunEvent::RunFinished {
                summary: RunSummary {
                    failed: vec![id("a")],
                    duration: Duration::from_secs(1),
                    ..RunSummary::default()
                },
            },
            Instant::now(),
        );
        state.apply(&RunEvent::WatchWaiting { files: 3 }, Instant::now());
        assert!(
            banner(&state, 80)
                .unwrap()
                .to_string()
                .starts_with("Last run failed in 1.0s.")
        );
    }
}
