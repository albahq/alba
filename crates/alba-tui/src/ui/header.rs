//! The header line: the run's live progress while `Phase::Running`, or a
//! one-line account of whatever else the session is doing — waiting on
//! watch, parked on a broken Beamfile, or the last run's outcome.
//!
//! This text is not rendered into its own pane: it sits inside the
//! outer frame's top border (see `ui/mod.rs`), the way the spec's
//! mockup draws it (`┌─ alba · run build ── ... ─┐`).

use std::time::Instant;

use alba_engine::RunSummary;
use ratatui::text::{Line, Span};

use crate::state::{AppState, Phase};

use super::format_duration;
use super::theme::{bar_style, outcome_style, parked_style};

/// The bar's width in cells. Matches the spec's mockup exactly: at 2/5
/// done the bar shows 6 filled cells of 14 (`round(2.0 / 5.0 * 14.0) ==
/// 6`), so this constant is not a free choice — it is the one the mockup
/// was drawn with.
const BAR_WIDTH: usize = 14;

pub fn line(state: &AppState, now: Instant) -> Line<'static> {
    match &state.phase {
        Phase::Running { done, total, since } => {
            let elapsed = now.saturating_duration_since(*since);
            let filled = filled_cells(*done, *total);
            let rest = BAR_WIDTH - filled;
            Line::from(vec![
                Span::raw(format!("alba · run {} ── ", state.target)),
                Span::styled("▰".repeat(filled), bar_style(state.colour)),
                Span::raw(format!(
                    "{} {done}/{total} · {}",
                    "▱".repeat(rest),
                    format_duration(elapsed)
                )),
            ])
        }
        Phase::Waiting { files } => {
            let plural = if *files == 1 { "" } else { "s" };
            Line::from(format!("alba · waiting · {files} file{plural} watched"))
        }
        // A single styled span, not `Line::styled` (which sets the
        // line's own style rather than a span's): `framed_title`
        // (`ui/mod.rs`) only carries a line's spans into the border's
        // title, so the colour has to live on the span to survive there.
        Phase::Parked => Line::from(vec![Span::styled(
            "alba · parked · waiting for a valid Beamfile".to_string(),
            parked_style(state.colour),
        )]),
        Phase::Finished => match &state.last_summary {
            Some(summary) => finished_line(&state.target, summary, state.colour),
            None => Line::from(format!("alba · {} · idle", state.target)),
        },
    }
}

/// Deterministic on purpose: `done`/`total` are the whole story, so the
/// same state always draws the same bar and no clock is involved in
/// deciding how full it looks — only in how long the run has taken,
/// which the header prints separately.
fn filled_cells(done: usize, total: usize) -> usize {
    if total == 0 {
        0
    } else {
        ((done as f64 / total as f64) * BAR_WIDTH as f64).round() as usize
    }
    .min(BAR_WIDTH)
}

/// The last run's outcome, once it is over. Zero buckets are omitted,
/// the same call the headless renderers' summary line makes (see
/// `alba-cli/src/render/mod.rs::summary_line`) — but the glyphs are the
/// tree pane's fixed four rather than the five-bucket breakdown text
/// renderers use, so a failed and an allowed-failure beam both read as
/// `✖` here, matching what the tree footer would have shown them as.
fn finished_line(target: &str, summary: &RunSummary, colour: bool) -> Line<'static> {
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
    Line::from(vec![
        Span::raw(format!("alba · run {target} finished · ")),
        Span::styled(counts_text, outcome_style(summary, colour)),
        Span::raw(format!(" · {}", format_duration(summary.duration))),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::RunSummary;
    use ratatui::style::{Color, Style};
    use std::time::Duration;

    fn state_with_phase(phase: Phase) -> AppState {
        let mut state = AppState::new("build", false);
        state.phase = phase;
        state
    }

    /// The spec's own worked example: 2/5 done draws 6 of 14 cells filled.
    #[test]
    fn running_shows_the_bar_and_the_elapsed_time() {
        let now = Instant::now();
        let state = state_with_phase(Phase::Running {
            done: 2,
            total: 5,
            since: now - Duration::from_secs(3),
        });
        assert_eq!(
            line(&state, now).to_string(),
            format!(
                "alba · run build ── {}{} 2/5 · 3.0s",
                "▰".repeat(6),
                "▱".repeat(8)
            )
        );
    }

    #[test]
    fn waiting_names_the_watched_file_count() {
        let state = state_with_phase(Phase::Waiting { files: 3 });
        assert_eq!(
            line(&state, Instant::now()).to_string(),
            "alba · waiting · 3 files watched"
        );
    }

    #[test]
    fn parked_names_the_broken_beamfile() {
        let state = state_with_phase(Phase::Parked);
        assert_eq!(
            line(&state, Instant::now()).to_string(),
            "alba · parked · waiting for a valid Beamfile"
        );
    }

    #[test]
    fn an_idle_session_with_no_summary_reads_idle() {
        let state = state_with_phase(Phase::Finished);
        assert_eq!(
            line(&state, Instant::now()).to_string(),
            "alba · build · idle"
        );
    }

    #[test]
    fn a_finished_run_reports_its_non_zero_buckets() {
        let mut state = state_with_phase(Phase::Finished);
        state.last_summary = Some(RunSummary {
            succeeded: vec![BeamId("a".into())],
            failed: vec![BeamId("b".into())],
            duration: Duration::from_secs(2),
            ..RunSummary::default()
        });
        assert_eq!(
            line(&state, Instant::now()).to_string(),
            "alba · run build finished · ✔ 1  ✖ 1 · 2.0s"
        );
    }

    /// `TestBackend::to_string()` (the render snapshots) drops styles, so
    /// this is what actually proves the bar, the outcome, and the parked
    /// text reach their colour functions with `state.colour` — the
    /// colour-to-status mapping itself is `theme.rs`'s own to pin.
    #[test]
    fn colour_reaches_the_bar_the_outcome_and_the_parked_spans() {
        let mut running = state_with_phase(Phase::Running {
            done: 1,
            total: 2,
            since: Instant::now(),
        });
        running.colour = true;
        assert_eq!(
            line(&running, Instant::now()).spans[1].style,
            Style::new().fg(Color::Green)
        );

        let mut parked = state_with_phase(Phase::Parked);
        parked.colour = true;
        assert_eq!(
            line(&parked, Instant::now()).spans[0].style,
            Style::new().fg(Color::Yellow)
        );

        let mut finished = state_with_phase(Phase::Finished);
        finished.colour = true;
        finished.last_summary = Some(RunSummary {
            succeeded: vec![BeamId("a".into())],
            ..RunSummary::default()
        });
        assert_eq!(
            line(&finished, Instant::now()).spans[1].style,
            Style::new().fg(Color::Green)
        );
    }
}
