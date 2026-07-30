//! The header line: the run's live progress while `Phase::Running`, or a
//! one-line account of whatever else the session is doing — waiting on
//! watch, parked on a broken Beamfile, or the last run's outcome.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use alba_engine::RunSummary;

use crate::state::{AppState, Phase};

use super::format_duration;

/// The bar's width in cells. Matches the spec's mockup exactly: at 2/5
/// done the bar shows 6 filled cells of 14 (`round(2.0 / 5.0 * 14.0) ==
/// 6`), so this constant is not a free choice — it is the one the mockup
/// was drawn with.
const BAR_WIDTH: usize = 14;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, now: Instant) {
    let text = match &state.phase {
        Phase::Running { done, total, since } => {
            let elapsed = now.saturating_duration_since(*since);
            format!(
                "alba · run {} ── {} {done}/{total} · {}",
                state.target,
                bar(*done, *total),
                format_duration(elapsed)
            )
        }
        Phase::Waiting { files } => {
            let plural = if *files == 1 { "" } else { "s" };
            format!("alba · waiting · {files} file{plural} watched")
        }
        Phase::Parked => "alba · parked · waiting for a valid Beamfile".to_string(),
        Phase::Finished => match &state.last_summary {
            Some(summary) => finished_line(&state.target, summary),
            None => format!("alba · {} · idle", state.target),
        },
    };
    frame.render_widget(Paragraph::new(text), area);
}

/// Deterministic on purpose: `done`/`total` are the whole story, so the
/// same state always draws the same bar and no clock is involved in
/// deciding how full it looks — only in how long the run has taken,
/// which the header prints separately.
fn bar(done: usize, total: usize) -> String {
    let filled = if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * BAR_WIDTH as f64).round() as usize
    }
    .min(BAR_WIDTH);
    format!("{}{}", "▰".repeat(filled), "▱".repeat(BAR_WIDTH - filled))
}

/// The last run's outcome, once it is over. Zero buckets are omitted,
/// the same call the headless renderers' summary line makes (see
/// `alba-cli/src/render/mod.rs::summary_line`) — but the glyphs are the
/// tree pane's fixed four rather than the five-bucket breakdown text
/// renderers use, so a failed and an allowed-failure beam both read as
/// `✖` here, matching what the tree footer would have shown them as.
fn finished_line(target: &str, summary: &RunSummary) -> String {
    let failed = summary.failed.len() + summary.failed_allowed.len();
    let counts = [
        ("✔", summary.succeeded.len()),
        ("⚡", summary.cached.len()),
        ("✖", failed),
        ("○", summary.cancelled.len()),
    ];
    let parts: Vec<String> = counts
        .iter()
        .filter(|(_, count)| *count > 0)
        .map(|(glyph, count)| format!("{glyph} {count}"))
        .collect();
    let counts_text = if parts.is_empty() {
        "nothing ran".to_string()
    } else {
        parts.join("  ")
    };
    format!(
        "alba · run {target} finished · {counts_text} · {}",
        format_duration(summary.duration)
    )
}
