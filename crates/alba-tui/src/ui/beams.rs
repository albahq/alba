//! The left pane: a title row with the selection counter, then one row
//! per beam (glyph at column 4, name at column 6, duration right-aligned
//! against the divider), the selected row on the accent. When the list
//! outgrows the pane it scrolls, and the divider doubles as the
//! scrollbar. The counts by status sit on the progress row (see
//! `ui/progress.rs`).

use std::ops::Range;
use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use alba_engine::BeamStatus;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::state::{AppState, BeamRow, BeamState, Mode, Phase};

use super::format_duration;
use super::theme::{ColourMode, Token, fill, status_token, style};

/// The cells around the name and the duration: the left margin (2), the
/// glyph (1), a gap after it (1) and a gap before the duration (1).
const FIXED_CHROME: usize = 5;

/// The cells the longest name shares the pane with: `FIXED_CHROME` and
/// the duration column (`DURATION_WIDTH`).
const CHROME_WIDTH: usize = FIXED_CHROME + DURATION_WIDTH;

/// The duration column, right-aligned. Seven cells hold every duration
/// up to `99m 59s` (`4.1s`, `2m 14s`); a longer one (`100m 10s` is
/// eight cells) takes the extra cells from the name on that row instead
/// of overflowing, so the right edge never moves.
const DURATION_WIDTH: usize = 7;

/// The pane never gets narrower than this.
const MIN_WIDTH: usize = 20;

/// The pane never takes more than this share of the frame, in fifths.
const MAX_SHARE_FIFTHS: usize = 2;

/// Drawn where a beam has no duration yet (U+2014).
const NO_DURATION: &str = "\u{2014}";

/// The pane's content width: the longest name plus `CHROME_WIDTH`,
/// capped at 40 percent of the frame, never under 20 cells.
pub fn beams_width(names: &[&str], frame_width: u16) -> u16 {
    let longest = names.iter().map(|name| name.width()).max().unwrap_or(0);
    let cap = frame_width as usize * MAX_SHARE_FIFTHS / 5;
    (longest + CHROME_WIDTH).min(cap).max(MIN_WIDTH) as u16
}

/// The rows of a `len`-long list shown in `height` rows with `selected`
/// in the middle, so it always has a row of context on each side except
/// against the ends of the list. The window is a function of the
/// selection alone: minimal-scroll behaviour (move only at the edges)
/// would need a scroll offset kept in the state, which this interface
/// does not carry.
pub fn window(selected: usize, len: usize, height: usize) -> Range<usize> {
    if len == 0 || height == 0 {
        return 0..0;
    }
    if len <= height {
        return 0..len;
    }
    let start = selected.saturating_sub(height / 2).min(len - height);
    start..start + height
}

/// The rail rows (`0..height`) the thumb covers for `window` of `len`
/// rows, `None` when everything fits. The thumb is proportional to the
/// window; at the list's end it always reaches the rail's end, and
/// rounding may bring it there a few rows earlier.
pub fn scrollbar(window: Range<usize>, len: usize, height: usize) -> Option<Range<usize>> {
    if len <= height || height == 0 {
        return None;
    }
    let round = |numerator: usize| (numerator * 2 + len) / (2 * len);
    let size = round(height * height).clamp(1, height);
    let start = if window.end >= len {
        height - size
    } else {
        round(window.start * height).min(height - size)
    };
    Some(start..start + size)
}

/// `selected/total`, one-based, without zero padding.
fn counter(selected: usize, total: usize) -> String {
    if total == 0 {
        return "0/0".to_string();
    }
    format!("{}/{total}", selected + 1)
}

/// The title's words: `BEAMS`, or `BEAMS / last run` while a watch
/// session waits between runs (or is parked) and the list is the last
/// run's.
fn title_text(state: &AppState) -> &'static str {
    match state.phase {
        Phase::Waiting { .. } | Phase::Parked => "BEAMS / last run",
        Phase::Running { .. } | Phase::Finished => "BEAMS",
    }
}

fn title_line(state: &AppState, width: usize) -> Line<'static> {
    let mode = state.colour;
    let counter = counter(highlighted(state), state.beams.len());
    let room = width.saturating_sub(counter.width() + 1);
    let title = title_text(state);
    let title = if title.width() <= room {
        title
    } else {
        "BEAMS"
    };
    let (name, suffix) = title.split_at("BEAMS".len());
    let pad = width.saturating_sub(title.width() + counter.width());
    Line::from(vec![
        Span::styled(name.to_string(), style(Token::Text, mode).bold()),
        Span::styled(suffix.to_string(), style(Token::TextMuted, mode)),
        Span::raw(" ".repeat(pad)),
        Span::styled(counter, style(Token::TextMuted, mode)),
    ])
}

/// The row on the selection bar: the selected beam, or in the graph the
/// focused one, which `Enter` makes the selection (`Esc` leaves the
/// selection where it was).
fn highlighted(state: &AppState) -> usize {
    match &state.mode {
        Mode::Graph(graph) => graph.focused,
        _ => state.selected,
    }
}

/// Draws the pane into `area` (the divider's left neighbour). The
/// scrollbar's thumb goes on the divider column right of `area`.
pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, now: Instant) {
    // A blank row under the title while the output pane has its
    // definition row: the list then starts level with the output's
    // blank separator row, one row above its first line.
    let blank = u16::from(super::output::has_definition_row(area.height));
    let rows = Layout::vertical([
        Constraint::Length(1),     // title row
        Constraint::Length(blank), // blank row
        Constraint::Min(0),        // one row per beam
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(title_line(state, area.width as usize)),
        rows[0],
    );

    let list = rows[2];
    let height = list.height as usize;
    let highlighted = highlighted(state);
    let visible = window(highlighted, state.beams.len(), height);
    for (offset, index) in visible.clone().enumerate() {
        let row = &state.beams[index];
        let line = row_line(
            row,
            state.is_skipped(row),
            now,
            area.width as usize,
            index == highlighted,
            state.colour,
        );
        let rect = Rect::new(list.x, list.y + offset as u16, list.width, 1);
        frame.buffer_mut().set_style(rect, line.style);
        frame.render_widget(Paragraph::new(line), rect);
    }

    if let Some(thumb) = scrollbar(visible, state.beams.len(), height) {
        let muted = style(Token::TextMuted, state.colour);
        for row in thumb {
            if let Some(cell) = frame
                .buffer_mut()
                .cell_mut((list.right(), list.y + row as u16))
            {
                cell.set_symbol("\u{2503}");
                cell.set_style(muted);
            }
        }
    }
}

/// The selection bar: `on-accent` bold on `accent`, reversed bold in
/// monochrome. Shared with the graph's focused node.
pub(super) fn selection_style(mode: ColourMode) -> Style {
    let bar = match mode {
        ColourMode::Monochrome => Style::new().add_modifier(Modifier::REVERSED),
        _ => fill(Token::Accent, mode).patch(style(Token::OnAccent, mode)),
    };
    bar.add_modifier(Modifier::BOLD)
}

/// One row: the glyph in its status colour, the name (muted while the
/// beam is pending, cancelled or skipped), the muted duration (none for
/// a `skipped` beam); the whole row on the selection bar when selected.
fn row_line(
    row: &BeamRow,
    skipped: bool,
    now: Instant,
    width: usize,
    selected: bool,
    colour: ColourMode,
) -> Line<'static> {
    let glyph = glyph_for(&row.state);
    let duration = duration_text(&row.state, skipped, now);
    let duration_width = duration.width().max(DURATION_WIDTH);
    // Budgeted by rendered width: a wide character paints two cells and
    // would otherwise push the duration out of its column.
    let name_width = width.saturating_sub(FIXED_CHROME + duration_width);
    let name = fit_name(&row.id, name_width);
    let pad = name_width.saturating_sub(name.width());
    let name_token = match &row.state {
        BeamState::Pending
        | BeamState::Done {
            status: BeamStatus::Cancelled,
            ..
        } => Token::TextMuted,
        BeamState::Running { .. } | BeamState::Done { .. } => Token::Text,
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(glyph, style(status_token(&row.state), colour)),
        Span::raw(" "),
        Span::styled(format!("{name}{:pad$}", ""), style(name_token, colour)),
        Span::raw(" "),
        Span::styled(
            format!("{duration:>duration_width$}"),
            style(Token::TextMuted, colour),
        ),
    ];
    if !selected {
        return Line::from(spans);
    }
    for span in &mut spans {
        span.style = Style::default();
    }
    Line::from(spans).style(selection_style(colour))
}

/// `id` cut to fit `width` rendered cells, with a trailing `…` when it
/// does not, so the duration column stays where it is. Beam ids may
/// contain any Unicode alphanumeric character (`alba-syntax`'s
/// `scan_ident` does not restrict them to ASCII), and a wide one paints
/// two cells, so this budgets by display width rather than char count.
pub(super) fn fit_name(id: &str, width: usize) -> String {
    if id.width() <= width {
        return id.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let budget = width - 1; // reserve one cell for the ellipsis
    let mut kept = String::new();
    let mut used = 0;
    for c in id.chars() {
        let cell = c.width().unwrap_or(0);
        if used + cell > budget {
            break;
        }
        kept.push(c);
        used += cell;
    }
    format!("{kept}…")
}

/// Shared with `ui/graphpane.rs`: the graph view's nodes use the exact
/// same status glyphs as the beams' rows, so this is the one place that
/// decides what each `BeamState` draws as. Every glyph is one cell.
pub(crate) fn glyph_for(state: &BeamState) -> &'static str {
    match state {
        BeamState::Pending => "○",
        BeamState::Running { .. } => "▶",
        BeamState::Done { status, .. } => status_glyph(status),
    }
}

fn status_glyph(status: &BeamStatus) -> &'static str {
    match status {
        BeamStatus::Succeeded => "✓",
        BeamStatus::Cached => "↯",
        BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => "✗",
        BeamStatus::Cancelled => "○",
    }
}

/// The duration column: a running beam's ticks live (`now - since`); a
/// pending beam has none yet, and one the engine skipped none at all (it
/// never ran, whatever duration it reports), so both show the dash.
fn duration_text(state: &BeamState, skipped: bool, now: Instant) -> String {
    match state {
        BeamState::Pending => NO_DURATION.to_string(),
        BeamState::Done { .. } if skipped => NO_DURATION.to_string(),
        BeamState::Running { since } => format_duration(now.saturating_duration_since(*since)),
        BeamState::Done { duration, .. } => format_duration(*duration),
    }
}

#[cfg(test)]
mod tests {
    use super::super::theme::Token;
    use super::*;
    use crate::state::Phase;
    use ratatui::style::Modifier;
    use std::time::Duration;

    #[test]
    fn a_failed_row_carries_the_status_colour_on_its_glyph() {
        let row = BeamRow {
            id: "build".to_string(),
            state: BeamState::Done {
                status: BeamStatus::Failed { exit_code: 1 },
                duration: Duration::from_secs(1),
            },
        };
        let line = row_line(&row, false, Instant::now(), 30, false, ColourMode::Indexed);
        assert_eq!(
            line.spans[1].style,
            style(Token::Error, ColourMode::Indexed)
        );
        assert_eq!(line.spans[1].content, "✗");
        let selected = row_line(&row, false, Instant::now(), 30, true, ColourMode::Indexed);
        assert_eq!(selected.style, selection_style(ColourMode::Indexed));
    }

    #[test]
    fn a_name_wider_than_its_column_is_truncated_with_an_ellipsis() {
        assert_eq!(
            fit_name("services:payment:integration", 19),
            "services:payment:i…"
        );
        assert_eq!(fit_name("build", 19), "build");
        assert_eq!(fit_name("abc", 0), "");
        assert_eq!(fit_name("abc", 1), "…");
    }

    #[test]
    fn a_name_exactly_as_wide_as_its_column_is_unchanged() {
        assert_eq!(fit_name("abcde", 5), "abcde");
    }

    #[test]
    fn a_name_one_cell_over_its_column_is_truncated() {
        assert_eq!(fit_name("abcdef", 5), "abcd…");
    }

    /// Beam ids may contain wide (double-cell) characters (`scan_ident`
    /// in `alba-syntax` is not ASCII-only), so `fit_name` budgets by
    /// display width: 5 wide chars (10 cells) truncated to 7 keeps only
    /// as many whole wide chars as fit alongside the ellipsis.
    #[test]
    fn a_wide_character_name_is_truncated_by_display_width_not_char_count() {
        assert_eq!(fit_name("国国国国国", 7), "国国国…");
    }

    fn names_row(id: &str, state: BeamState) -> BeamRow {
        BeamRow {
            id: id.to_string(),
            state,
        }
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn the_width_follows_the_longest_name_within_bounds() {
        assert_eq!(beams_width(&["fmt"], 104), 20);
        assert_eq!(beams_width(&["services:payment:integration"], 104), 40);
        let long = "a".repeat(60);
        assert_eq!(beams_width(&[long.as_str()], 60), 24);
        assert_eq!(beams_width(&["a", "check:everything"], 104), 28);
        assert_eq!(beams_width(&["国国国国国国国国"], 104), 28);
        assert_eq!(beams_width(&[], 40), 20);
    }

    #[test]
    fn the_window_keeps_the_selection_with_context() {
        assert_eq!(window(13, 48, 13), 7..20);
        assert_eq!(window(30, 48, 13), 24..37);
        let range = window(30, 48, 13);
        assert!(range.contains(&29) && range.contains(&31));
        assert_eq!(window(2, 5, 10), 0..5, "a list that fits is whole");
    }

    #[test]
    fn window_at_the_ends() {
        assert_eq!(window(0, 48, 13), 0..13);
        assert_eq!(window(47, 48, 13), 35..48);
        assert_eq!(window(0, 0, 13), 0..0);
        assert_eq!(window(5, 48, 0), 0..0);
        assert_eq!(window(5, 48, 1), 5..6);
    }

    #[test]
    fn the_thumb_is_proportional() {
        assert_eq!(scrollbar(7..20, 48, 13), Some(2..6));
        assert_eq!(scrollbar(0..13, 48, 13), Some(0..4));
        assert_eq!(
            scrollbar(35..48, 48, 13),
            Some(9..13),
            "ends on the rail's end"
        );
        assert_eq!(scrollbar(0..13, 13, 13), None);
        assert_eq!(scrollbar(0..5, 5, 13), None);
        assert_eq!(
            scrollbar(0..3, 1000, 3),
            Some(0..1),
            "never thinner than one row"
        );
    }

    #[test]
    fn the_counter_reads_selection_over_total() {
        assert_eq!(counter(2, 4), "3/4");
        assert_eq!(counter(21, 48), "22/48");
        assert_eq!(counter(0, 0), "0/0");
    }

    #[test]
    fn a_row_lays_glyph_name_and_duration_on_the_columns() {
        let row = names_row(
            "build",
            BeamState::Done {
                status: BeamStatus::Succeeded,
                duration: Duration::from_millis(4100),
            },
        );
        let line = row_line(
            &row,
            false,
            Instant::now(),
            24,
            false,
            ColourMode::TrueColour,
        );
        assert_eq!(text_of(&line), "  ✓ build           4.1s");
        assert_eq!(
            line.spans[3].style,
            style(Token::Text, ColourMode::TrueColour)
        );
        let pending = names_row("test", BeamState::Pending);
        let line = row_line(
            &pending,
            false,
            Instant::now(),
            24,
            false,
            ColourMode::TrueColour,
        );
        assert_eq!(text_of(&line), "  ○ test               —");
        assert_eq!(
            line.spans[3].style,
            style(Token::TextMuted, ColourMode::TrueColour)
        );
    }

    #[test]
    fn a_running_duration_ticks_without_an_ellipsis() {
        let now = Instant::now();
        let row = names_row(
            "build",
            BeamState::Running {
                since: now - Duration::from_millis(2500),
            },
        );
        let text = text_of(&row_line(
            &row,
            false,
            now,
            24,
            false,
            ColourMode::Monochrome,
        ));
        assert!(text.ends_with("   2.5s"), "{text:?}");
    }

    #[test]
    fn every_duration_fits_and_stays_right_aligned() {
        for secs in [0, 59, 134, 6_010, 7_384, 60_000, 600_000] {
            let row = names_row(
                "build",
                BeamState::Done {
                    status: BeamStatus::Succeeded,
                    duration: Duration::from_secs(secs),
                },
            );
            let text = text_of(&row_line(
                &row,
                false,
                Instant::now(),
                20,
                false,
                ColourMode::Monochrome,
            ));
            assert_eq!(text.width(), 20, "{secs}s: {text:?}");
            assert!(
                text.ends_with(&format_duration(Duration::from_secs(secs))),
                "{secs}s: {text:?}"
            );
        }
    }

    #[test]
    fn the_selected_row_is_on_accent_bold_and_whole() {
        let row = names_row("build", BeamState::Pending);
        let line = row_line(
            &row,
            false,
            Instant::now(),
            24,
            true,
            ColourMode::TrueColour,
        );
        let expected = fill(Token::Accent, ColourMode::TrueColour)
            .patch(style(Token::OnAccent, ColourMode::TrueColour))
            .add_modifier(Modifier::BOLD);
        assert_eq!(line.style, expected);
        assert!(line.spans.iter().all(|s| s.style == Style::default()));
        assert_eq!(text_of(&line).width(), 24);
        let mono = row_line(
            &row,
            false,
            Instant::now(),
            24,
            true,
            ColourMode::Monochrome,
        );
        assert_eq!(
            mono.style,
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        );
    }

    #[test]
    fn the_title_names_the_last_run_between_watch_runs() {
        let mut state = AppState::new("build", true);
        assert_eq!(title_text(&state), "BEAMS");
        state.phase = Phase::Waiting { files: 3 };
        assert_eq!(title_text(&state), "BEAMS / last run");
    }

    #[test]
    fn a_wide_name_keeps_the_duration_column() {
        let row = names_row(
            &"国".repeat(10),
            BeamState::Done {
                status: BeamStatus::Cached,
                duration: Duration::from_millis(1200),
            },
        );
        let line = row_line(
            &row,
            false,
            Instant::now(),
            30,
            false,
            ColourMode::Monochrome,
        );
        let rendered = text_of(&line);
        assert_eq!(rendered.width(), 30);
        assert!(rendered.ends_with("   1.2s"), "{rendered:?}");
        assert!(rendered.contains('…'));
    }

    /// A skipped beam never ran, so it has no duration to show, even
    /// though the engine reports one; a beam cancelled while running
    /// keeps the time it ran.
    #[test]
    fn a_skipped_beam_shows_no_duration() {
        use alba_core::BeamId;
        use alba_engine::RunEvent;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let id = |name: &str| BeamId(name.to_string());
        let now = Instant::now();
        let mut state = AppState::new("check", false);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id("check")],
                affected_by: None,
                beams: vec![id("test"), id("check")],
                edges: vec![(id("check"), id("test"))],
                details: Vec::new(),
            },
            now,
        );
        state.apply(&RunEvent::BeamStarted { id: id("test") }, now);
        for name in ["test", "check"] {
            state.apply(
                &RunEvent::BeamFinished {
                    id: id(name),
                    status: BeamStatus::Cancelled,
                    duration: Duration::from_millis(2000),
                },
                now,
            );
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 20, 10), &state, now))
            .unwrap();
        let row = |y: u16| {
            (0..20)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        let rows: Vec<String> = (0..10).map(row).collect();
        let test = rows.iter().find(|row| row.contains("test")).unwrap();
        let check = rows.iter().find(|row| row.contains("check")).unwrap();
        assert!(test.trim_end().ends_with("2.0s"), "{test:?}");
        assert!(check.trim_end().ends_with(NO_DURATION), "{check:?}");
    }

    /// Where the definition row is drawn, the beams pane leaves the row
    /// under its title empty, so the list starts level with the output's
    /// blank separator row; under 16 rows both give their rows back.
    #[test]
    fn the_list_starts_under_a_blank_row_when_the_output_has_its_definition() {
        use alba_core::BeamId;
        use alba_engine::RunEvent;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut state = AppState::new("build", false);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![BeamId("build".to_string())],
                affected_by: None,
                beams: vec![BeamId("build".to_string())],
                edges: Vec::new(),
                details: Vec::new(),
            },
            Instant::now(),
        );
        let first_row = |height: u16| {
            let mut terminal = Terminal::new(TestBackend::new(60, height)).unwrap();
            terminal
                .draw(|frame| super::super::draw(frame, &state, Instant::now()))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (5..height)
                .find(|&y| {
                    (2..20)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains("build")
                })
                .unwrap()
        };
        // The title sits on row 5, the first body row.
        assert_eq!(first_row(16), 7);
        assert_eq!(first_row(15), 6);
    }
}
