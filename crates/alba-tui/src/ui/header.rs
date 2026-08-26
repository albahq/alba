//! The top edge's two titles: the run's identity on the left (`alba ·
//! {target}`), and, on the right, what the session is doing besides the
//! run, when there is anything to say: a watch waiting between runs,
//! or a project parked on a broken Beamfile. The run's own progress and
//! outcome are the footer's (see `ui/footer.rs`), so a watch never has
//! to evict them to say it is watching.
//!
//! Neither text is rendered into its own pane: both sit inside the
//! outer frame's top border (see `ui/mod.rs`), the way the spec's
//! mockup draws them (`┌─ alba · build ──── ... ── watching 3 files ─┐`).

use ratatui::text::{Line, Span};

use crate::state::{AppState, Phase};

use super::theme::parked_style;

/// The left-hand title.
pub fn line(state: &AppState) -> Line<'static> {
    Line::from(format!("alba · {}", state.target))
}

/// The right-hand title, when the session has a state of its own to
/// report. A single styled span, not `Line::styled` (which sets the
/// line's own style rather than a span's): `framed_title` (`ui/mod.rs`)
/// only carries a line's spans into the border's title, so the colour
/// has to live on the span to survive there.
pub fn session(state: &AppState) -> Option<Line<'static>> {
    match &state.phase {
        Phase::Waiting { files } => {
            let plural = if *files == 1 { "" } else { "s" };
            Some(Line::from(format!("watching {files} file{plural}")))
        }
        Phase::Parked => Some(Line::from(vec![Span::styled(
            "parked · waiting for a valid Beamfile".to_string(),
            parked_style(state.colour),
        )])),
        Phase::Running { .. } | Phase::Finished => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use std::time::Instant;

    fn state_with_phase(phase: Phase) -> AppState {
        let mut state = AppState::new("build", false);
        state.phase = phase;
        state
    }

    #[test]
    fn the_left_title_is_the_identity_whatever_the_phase() {
        let running = state_with_phase(Phase::Running {
            done: 2,
            total: 5,
            since: Instant::now(),
        });
        assert_eq!(line(&running).to_string(), "alba · build");
        assert_eq!(
            line(&state_with_phase(Phase::Finished)).to_string(),
            "alba · build"
        );
    }

    #[test]
    fn the_session_title_is_absent_while_running_or_finished() {
        assert!(session(&state_with_phase(Phase::Finished)).is_none());
        assert!(
            session(&state_with_phase(Phase::Running {
                done: 0,
                total: 1,
                since: Instant::now(),
            }))
            .is_none()
        );
    }

    #[test]
    fn a_waiting_watch_names_its_file_count() {
        assert_eq!(
            session(&state_with_phase(Phase::Waiting { files: 3 }))
                .unwrap()
                .to_string(),
            "watching 3 files"
        );
        assert_eq!(
            session(&state_with_phase(Phase::Waiting { files: 1 }))
                .unwrap()
                .to_string(),
            "watching 1 file"
        );
    }

    /// `TestBackend::to_string()` (the render snapshots) drops styles, so
    /// this is what proves the parked text reaches its colour function
    /// with `state.colour`.
    #[test]
    fn a_parked_project_is_named_in_the_parked_colour() {
        let mut parked = state_with_phase(Phase::Parked);
        parked.colour = true;
        let title = session(&parked).unwrap();
        assert_eq!(title.to_string(), "parked · waiting for a valid Beamfile");
        assert_eq!(title.spans[0].style, Style::new().fg(Color::Yellow));
    }
}
