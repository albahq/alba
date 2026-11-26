//! The header row: the ` ALBA ` badge and the `mode / target`
//! breadcrumb on the left, the session state and its detail on the
//! right, ending at the content's last column.
//!
//! When the row is too narrow the detail goes first, then the target is
//! cut with `…`. The badge and the state are never cut.

use std::time::Instant;

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::state::{AppState, Phase};

use super::beams::fit_name;
use super::format_duration;
use super::theme::{ColourMode, Token, fill, style};

/// The fewest blank cells kept between the breadcrumb and the state.
const GAP: usize = 2;
/// Between the state and its detail.
const DETAIL_GAP: &str = "   ";
const BADGE: &str = " ALBA ";

/// The ` ALBA ` badge: `on-accent` bold on `accent`, reversed bold in
/// monochrome. Shared with the too-small screen.
pub(super) fn badge(mode: ColourMode) -> Span<'static> {
    let style = match mode {
        ColourMode::Monochrome => Style::new().reversed(),
        _ => fill(Token::Accent, mode).patch(style(Token::OnAccent, mode)),
    };
    Span::styled(BADGE, style.bold())
}

/// What the session is doing, as the header's right side reports it.
enum Session {
    Idle,
    Running,
    Passed,
    Failed,
    Cancelled,
    Watching,
    Parked,
}

impl Session {
    fn of(state: &AppState) -> Self {
        match &state.phase {
            Phase::Running { .. } => Session::Running,
            Phase::Waiting { .. } => Session::Watching,
            Phase::Parked => Session::Parked,
            Phase::Finished => match &state.last_summary {
                None => Session::Idle,
                Some(_) if state.run_cancelled() => Session::Cancelled,
                // An allowed failure is a failure here, as in the
                // outcome colour.
                Some(summary)
                    if !summary.failed.is_empty() || !summary.failed_allowed.is_empty() =>
                {
                    Session::Failed
                }
                Some(_) => Session::Passed,
            },
        }
    }

    fn label(&self) -> (&'static str, Token) {
        match self {
            Session::Idle => ("○ IDLE", Token::TextMuted),
            Session::Running => ("▶ RUNNING", Token::Accent),
            Session::Passed => ("✓ PASSED", Token::Success),
            Session::Failed => ("✗ FAILED", Token::Error),
            Session::Cancelled => ("○ CANCELLED", Token::TextMuted),
            Session::Watching => ("● WATCHING", Token::Success),
            Session::Parked => ("Ⅱ PARKED", Token::Accent),
        }
    }

    /// The elapsed or total time, or the watched-file count.
    fn detail(&self, state: &AppState, now: Instant) -> Option<String> {
        match (self, &state.phase) {
            (Session::Running, Phase::Running { since, .. }) => {
                Some(format_duration(now.saturating_duration_since(*since)))
            }
            (Session::Passed | Session::Failed | Session::Cancelled, _) => state
                .last_summary
                .as_ref()
                .map(|summary| format_duration(summary.duration)),
            (Session::Watching, Phase::Waiting { files }) => {
                let plural = if *files == 1 { "" } else { "s" };
                Some(format!("{files} file{plural}"))
            }
            _ => None,
        }
    }
}

/// The header for a row `width` cells wide.
pub fn line(state: &AppState, now: Instant, width: u16) -> Line<'static> {
    let mode = state.colour;
    let width = width as usize;
    let session = Session::of(state);
    let (label, token) = session.label();
    let breadcrumb_mode = if state.watch_enabled { "watch" } else { "run" };
    let fixed_left = BADGE.width() + 2 + breadcrumb_mode.width() + " / ".width();

    let mut detail = session.detail(state, now);
    let right_width = |detail: &Option<String>| {
        label.width()
            + detail
                .as_ref()
                .map_or(0, |detail| DETAIL_GAP.len() + detail.width())
    };
    let fits = |target: &str, detail: &Option<String>| {
        fixed_left + target.width() + GAP + right_width(detail) <= width
    };
    if !fits(&state.target, &detail) {
        detail = None;
    }
    let target = if fits(&state.target, &detail) {
        state.target.clone()
    } else {
        fit_name(
            &state.target,
            width.saturating_sub(fixed_left + GAP + right_width(&detail)),
        )
    };

    let left_width = fixed_left + target.width();
    let pad = width.saturating_sub(left_width + right_width(&detail));
    let mut spans = vec![
        badge(mode),
        Span::raw("  "),
        Span::styled(breadcrumb_mode, style(Token::TextMuted, mode)),
        Span::raw(" "),
        Span::styled("/", style(Token::Line, mode)),
        Span::raw(" "),
        Span::styled(target, style(Token::Text, mode).bold()),
        Span::raw(" ".repeat(pad)),
        Span::styled(label, style(token, mode).bold()),
    ];
    if let Some(detail) = detail {
        spans.push(Span::raw(DETAIL_GAP));
        spans.push(Span::styled(detail, style(Token::TextMuted, mode)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::{RunEvent, RunSummary};
    use std::time::Duration;

    fn started(target: &str, watch: bool, now: Instant) -> AppState {
        let mut state = AppState::new(target, watch);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![BeamId(target.to_string())],
                affected_by: None,
                base: None,
                beams: vec![BeamId(target.to_string())],
                edges: Vec::new(),
                details: Vec::new(),
            },
            now,
        );
        state
    }

    fn finished(state: &mut AppState, summary: RunSummary, now: Instant) {
        state.apply(&RunEvent::RunFinished { summary }, now);
    }

    fn summary_failing(allowed: bool) -> RunSummary {
        let failed = vec![BeamId("build".into())];
        RunSummary {
            failed: if allowed { Vec::new() } else { failed.clone() },
            failed_allowed: if allowed { failed } else { Vec::new() },
            duration: Duration::from_millis(4200),
            ..RunSummary::default()
        }
    }

    fn drawn(state: &AppState, now: Instant, width: u16) -> String {
        let line = line(state, now, width);
        assert!(line.width() <= width as usize, "{line} overflows {width}");
        line.to_string()
    }

    #[test]
    fn the_header_names_the_mode_target_and_state() {
        let now = Instant::now();
        let idle = AppState::new("build", false);
        assert_eq!(
            drawn(&idle, now, 60),
            format!(" ALBA   run / build{}○ IDLE", " ".repeat(35))
        );

        let running = started("build", false, now - Duration::from_millis(2300));
        assert_eq!(
            drawn(&running, now, 60),
            format!(" ALBA   run / build{}▶ RUNNING   2.3s", " ".repeat(25))
        );

        let mut passed = started("build", false, now);
        finished(
            &mut passed,
            RunSummary {
                succeeded: vec![BeamId("build".into())],
                duration: Duration::from_secs(134),
                ..RunSummary::default()
            },
            now,
        );
        assert!(drawn(&passed, now, 60).ends_with("✓ PASSED   2m 14s"));

        for allowed in [false, true] {
            let mut failed = started("build", false, now);
            finished(&mut failed, summary_failing(allowed), now);
            assert!(drawn(&failed, now, 60).ends_with("✗ FAILED   4.2s"));
        }

        let mut cancelled = started("build", false, now);
        cancelled.mark_user_cancelled();
        finished(
            &mut cancelled,
            RunSummary {
                cancelled: vec![BeamId("build".into())],
                duration: Duration::from_secs(2),
                ..RunSummary::default()
            },
            now,
        );
        assert!(drawn(&cancelled, now, 60).ends_with("○ CANCELLED   2.0s"));

        let mut watching = started("build", true, now);
        finished(&mut watching, RunSummary::default(), now);
        watching.apply(&RunEvent::WatchWaiting { files: 3 }, now);
        let text = drawn(&watching, now, 60);
        assert!(text.starts_with(" ALBA   watch / build"), "{text}");
        assert!(text.ends_with("● WATCHING   3 files"), "{text}");
        watching.apply(&RunEvent::WatchWaiting { files: 1 }, now);
        assert!(drawn(&watching, now, 60).ends_with("● WATCHING   1 file"));

        let mut parked = AppState::new("build", true);
        parked.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\n".to_string(),
                report: None,
            },
            now,
        );
        assert!(drawn(&parked, now, 60).ends_with("Ⅱ PARKED"));
    }

    #[test]
    fn a_narrow_header_drops_the_detail_then_cuts_the_target() {
        let now = Instant::now();
        let state = started("services:payment", false, now - Duration::from_secs(3));
        // " ALBA   run / services:payment" is 30, the right side 16.
        assert_eq!(
            drawn(&state, now, 48),
            " ALBA   run / services:payment  ▶ RUNNING   3.0s"
        );
        // One cell short: the detail goes, the target stays whole.
        assert_eq!(
            drawn(&state, now, 47),
            format!(" ALBA   run / services:payment{}▶ RUNNING", " ".repeat(8))
        );
        // Without the detail: 30 + 2 + 9 = 41; under it, the target is cut.
        assert_eq!(
            drawn(&state, now, 41),
            " ALBA   run / services:payment  ▶ RUNNING"
        );
        assert_eq!(
            drawn(&state, now, 36),
            " ALBA   run / services:p…  ▶ RUNNING"
        );
    }

    #[test]
    fn the_badge_and_state_carry_their_colours() {
        let now = Instant::now();
        let state = started("build", false, now).with_colour(ColourMode::TrueColour);
        let header = line(&state, now, 60);
        assert_eq!(
            header.spans[0].style,
            fill(Token::Accent, ColourMode::TrueColour)
                .patch(style(Token::OnAccent, ColourMode::TrueColour))
                .bold()
        );
        let running = header
            .spans
            .iter()
            .find(|span| span.content == "▶ RUNNING")
            .unwrap();
        assert_eq!(
            running.style,
            style(Token::Accent, ColourMode::TrueColour).bold()
        );

        let mono = started("build", false, now);
        assert_eq!(
            line(&mono, now, 60).spans[0].style,
            Style::new().reversed().bold()
        );
    }
}
