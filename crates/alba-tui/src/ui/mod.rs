//! The whole frame, band by band, for a terminal of `W` x `H` cells:
//! row 1 the header, row 3 the progress row (or the banner between watch
//! runs), row 4 the top separator, rows 5 to `H-4` the body (beams pane,
//! divider, output pane), row `H-3` the bottom separator, row `H-2` the
//! key hints; rows 0, 2 and `H-1` stay empty. There is no outer border:
//! content runs from column 2 to `W-3`, separators from column 1 to
//! `W-2`, with `┬`/`┴` where the divider meets them.
//!
//! The whole frame is painted with the surface first, so every cell the
//! interface draws, uncoloured command output included, reads on Alba's
//! own background in `text`.
//!
//! Nothing here samples a clock or touches the terminal: `now` arrives
//! as an argument, so a redraw is a deterministic function of its
//! inputs and a snapshot test owns the clock.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::state::{AppState, Mode};

use theme::{ColourMode, Token, fill, style};

mod graphpane;
mod header;
mod help;
mod keys;
mod logpane;
mod progress;
pub(crate) mod theme;
mod tree;

/// Below this floor the panes have no room left to mean anything, so the
/// whole screen gives way to the too-small screen.
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 12;

/// The tree pane's fixed content width, left of the divider.
const TREE_WIDTH: u16 = 30;

/// Rows above the body (empty, header, empty, progress, separator) and
/// below it (separator, key hints, empty).
const ROWS_ABOVE_BODY: u16 = 5;
const ROWS_BELOW_BODY: u16 = 3;

/// The body's rectangle for a terminal of `width` x `height` cells:
/// columns 2 to `W-3`, rows 5 to `H-4`. `None` below the 40x12 floor,
/// where the too-small screen is all there is.
pub fn body_area(width: u16, height: u16) -> Option<Rect> {
    if width < MIN_WIDTH || height < MIN_HEIGHT {
        return None;
    }
    Some(Rect::new(
        2,
        ROWS_ABOVE_BODY,
        width - 4,
        height - ROWS_ABOVE_BODY - ROWS_BELOW_BODY,
    ))
}

/// The body split into the tree pane (with the divider as its right
/// border) and the output pane. `draw` and `log_pane_content_area` both
/// go through here, so a click never drifts from what was painted.
fn panes(body: Rect) -> (Rect, Rect) {
    let panes =
        Layout::horizontal([Constraint::Length(TREE_WIDTH + 1), Constraint::Min(1)]).split(body);
    (panes[0], panes[1])
}

pub fn draw(frame: &mut Frame, state: &AppState, now: Instant) {
    let area = frame.area();
    let mode = state.colour;
    let surface = fill(Token::Surface, mode).patch(style(Token::Text, mode));
    frame.buffer_mut().set_style(area, surface);
    let Some(body) = body_area(area.width, area.height) else {
        draw_too_small(frame, area, mode);
        return;
    };
    let content_row = |y: u16| Rect::new(2, y, area.width - 4, 1);
    let width = area.width - 4;

    frame.render_widget(
        Paragraph::new(header::line(state, now, width)),
        content_row(1),
    );
    let progress = progress::banner(state, width).unwrap_or_else(|| progress::line(state, width));
    frame.render_widget(Paragraph::new(progress), content_row(3));
    let hints = match &state.last_copy_result {
        Some(result) if result.is_visible(now) => keys::copy_result(result.message, mode),
        _ => keys::line(state, width),
    };
    frame.render_widget(Paragraph::new(hints), content_row(area.height - 2));

    let top = ROWS_ABOVE_BODY - 1;
    let bottom = area.height - ROWS_BELOW_BODY;
    draw_separator(frame, area, top, mode);
    draw_separator(frame, area, bottom, mode);

    // Graph mode replaces the body entirely: no tree, no output pane,
    // no divider, so no junctions on the separators either.
    if let Mode::Graph(graph) = &state.mode {
        graphpane::draw(frame, body, state, graph);
        return;
    }

    let (tree_pane, output_pane) = panes(body);
    let divider = Block::new()
        .borders(Borders::RIGHT)
        .border_style(style(Token::Line, mode));
    let tree_area = divider.inner(tree_pane);
    frame.render_widget(divider, tree_pane);
    let divider_x = tree_pane.right() - 1;
    let buffer = frame.buffer_mut();
    if let Some(cell) = buffer.cell_mut((divider_x, top)) {
        cell.set_symbol("┬");
    }
    if let Some(cell) = buffer.cell_mut((divider_x, bottom)) {
        cell.set_symbol("┴");
    }

    tree::draw(frame, tree_area, state, now);
    logpane::draw(frame, output_pane, state);

    // Help is still an overlay over the body, the panes dimmed under it.
    if matches!(state.mode, Mode::Help) {
        help::draw(frame, body, surface);
    }
}

/// A separator row, `─` in `line` from column 1 to `W-2`.
fn draw_separator(frame: &mut Frame, area: Rect, y: u16, mode: ColourMode) {
    let width = area.width - 2;
    frame.render_widget(
        Paragraph::new("─".repeat(width as usize)).style(style(Token::Line, mode)),
        Rect::new(1, y, width, 1),
    );
}

/// Below the floor: the badge, `Terminal too small` bold, then the
/// size needed and the size there is, muted, centred.
fn draw_too_small(frame: &mut Frame, area: Rect, mode: ColourMode) {
    let muted = style(Token::TextMuted, mode);
    let lines = vec![
        Line::from(header::badge(mode)),
        Line::default(),
        Line::styled("Terminal too small", style(Token::Text, mode).bold()),
        Line::styled(
            format!("Resize to at least {MIN_WIDTH}×{MIN_HEIGHT}"),
            muted,
        ),
        Line::styled(format!("Now {}×{}", area.width, area.height), muted),
    ];
    let height = (lines.len() as u16).min(area.height);
    let block = Rect::new(
        area.x,
        area.y + (area.height - height) / 2,
        area.width,
        height,
    );
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), block);
}

/// How long something took: one decimal of a second (`4.1s`) under a
/// minute, minutes and whole seconds (`2m 14s`) from there. `alba`'s
/// own `format_duration` stays as it is: the headless output does not
/// change, and the dependency direction (`alba` to `alba-tui`) keeps
/// this crate from importing it.
pub(crate) fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    // `{:.1}` would read 59.96s as `60.0s`: from there on, minutes.
    if seconds < 59.95 {
        return format!("{seconds:.1}s");
    }
    let whole = seconds.round() as u64;
    format!("{}m {}s", whole / 60, whole % 60)
}

/// The log pane's own content rectangle, below its title row, for a
/// terminal of `width` x `height` cells. `None` below the too-small
/// floor, where there is no pane to hit-test against.
///
/// Copy mode's mouse handling (`lib::dispatch_mouse`) asks this rather
/// than re-deriving the layout its own way, so a click can never drift
/// out of sync with what `draw` actually painted.
pub fn log_pane_content_area(width: u16, height: u16) -> Option<Rect> {
    let (_, output_pane) = panes(body_area(width, height)?);
    let pane = Layout::vertical([
        Constraint::Length(1), // title row
        Constraint::Min(0),    // output
    ])
    .split(output_pane);
    Some(pane[1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::RunEvent;
    use alba_executors::{OutputLine, Stream};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;
    use unicode_width::UnicodeWidthStr;

    fn id(name: &str) -> BeamId {
        BeamId(name.to_string())
    }

    /// A run in flight with a wide (CJK) beam name and long output.
    fn running(now: Instant) -> AppState {
        let mut state = AppState::new("build", false);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id("build")],
                affected_by: None,
                beams: vec![
                    id("codegen"),
                    id("国国国国国国国国国国国国国国"),
                    id("build"),
                ],
                edges: vec![(id("build"), id("codegen"))],
                details: Vec::new(),
            },
            now,
        );
        state.apply(&RunEvent::BeamStarted { id: id("codegen") }, now);
        for index in 0..200 {
            state.apply(
                &RunEvent::BeamOutput {
                    id: id("codegen"),
                    line: OutputLine {
                        stream: if index % 3 == 0 {
                            Stream::Stderr
                        } else {
                            Stream::Stdout
                        },
                        text: format!("{index} {}", "compiling 国 crate ".repeat(index % 12)),
                    },
                    replayed: false,
                },
                now,
            );
        }
        state
    }

    fn drawn(state: &AppState, width: u16, height: u16, now: Instant) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state, now)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn body_area_leaves_the_bands_and_the_margins() {
        assert_eq!(body_area(80, 24), Some(Rect::new(2, 5, 76, 16)));
        assert_eq!(body_area(40, 12), Some(Rect::new(2, 5, 36, 4)));
        assert_eq!(body_area(MIN_WIDTH - 1, 24), None);
        assert_eq!(body_area(80, MIN_HEIGHT - 1), None);
    }

    #[test]
    fn log_pane_content_area_sits_past_the_tree_and_its_title() {
        // Body from column 2; the tree and the divider take 31 columns;
        // the title row takes one line.
        assert_eq!(
            log_pane_content_area(80, 24),
            Some(Rect::new(33, 6, 45, 15))
        );
        assert_eq!(log_pane_content_area(MIN_WIDTH - 1, 24), None);
    }

    #[test]
    fn the_bands_sit_on_their_rows() {
        let now = Instant::now();
        let buffer = drawn(&running(now), 80, 24, now);
        assert_eq!(row(&buffer, 0).trim(), "");
        assert!(row(&buffer, 1).starts_with("   ALBA   run / build"));
        assert_eq!(row(&buffer, 2).trim(), "");
        assert!(row(&buffer, 3).contains("0/3 complete"));
        assert_eq!(
            row(&buffer, 4),
            format!(" {}┬{} ", "─".repeat(31), "─".repeat(46))
        );
        assert_eq!(
            buffer[(32, 10)].symbol(),
            "│",
            "the divider runs down the body"
        );
        assert_eq!(
            row(&buffer, 21),
            format!(" {}┴{} ", "─".repeat(31), "─".repeat(46))
        );
        assert!(row(&buffer, 22).starts_with("  ↑↓ beam"));
        assert_eq!(row(&buffer, 23).trim(), "");
    }

    #[test]
    fn drawing_never_panics_from_40_to_200_columns() {
        use crate::graph::GraphState;
        use alba_engine::RunSummary;

        let now = Instant::now();
        let with_mode = |mode: Mode| {
            let mut state = running(now);
            state.mode = mode;
            state
        };
        let mut waiting = running(now);
        waiting.watch_enabled = true;
        waiting.apply(
            &RunEvent::RunFinished {
                summary: RunSummary::default(),
            },
            now,
        );
        waiting.apply(&RunEvent::WatchWaiting { files: 12 }, now);
        let mut parked = running(now);
        parked.watch_enabled = true;
        parked.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: unknown target `nope`\n".to_string(),
                report: None,
            },
            now,
        );
        let mut search = running(now);
        search.enter_search();
        if let Mode::Search(typed) = &mut search.mode {
            typed.query = "compiling 国 crate".to_string();
        }
        let states = [
            (running(now), "▶ RUNNING"),
            (with_mode(Mode::Help), "▶ RUNNING"),
            (with_mode(Mode::Graph(GraphState::new(0))), "▶ RUNNING"),
            (search, "▶ RUNNING"),
            (waiting, "● WATCHING"),
            (parked, "Ⅱ PARKED"),
        ];
        for height in [12, 24, 50] {
            for width in 40..=200 {
                let content = width as usize - 4;
                for (state, label) in &states {
                    let at = format!("{label} at {width}x{height}");
                    assert!(
                        header::line(state, now, width - 4).width() <= content,
                        "{at}"
                    );
                    let progress = progress::banner(state, width - 4)
                        .unwrap_or_else(|| progress::line(state, width - 4));
                    assert!(progress.width() <= content, "{at}");
                    assert!(keys::line(state, width - 4).width() <= content, "{at}");

                    let buffer = drawn(state, width, height, now);
                    let header = row(&buffer, 1);
                    let drawn_header = header.trim_end();
                    assert_eq!(
                        drawn_header.width(),
                        width as usize - 2,
                        "the header ends at column W-3, {at}: {header:?}"
                    );
                    assert!(
                        drawn_header.ends_with(label)
                            || drawn_header.contains(&format!("{label}   ")),
                        "the state closes the header, {at}: {header:?}"
                    );
                }
            }
        }
    }

    /// The badge is the one band on its own background. The cell behind
    /// the second half of a wide character is never drawn (ratatui
    /// resets it and the terminal paints the wide glyph over it), so it
    /// is not checked.
    #[test]
    fn the_frame_paints_the_surface_everywhere() {
        let now = Instant::now();
        let state = running(now).with_colour(ColourMode::TrueColour);
        let mut help = running(now).with_colour(ColourMode::TrueColour);
        help.mode = Mode::Help;
        for state in [&state, &help] {
            assert_surface_everywhere_but_the_badge(&drawn(state, 104, 23, now));
        }
        let buffer = drawn(&state, 104, 23, now);
        assert_eq!(buffer[(40, 12)].fg, Color::Rgb(0xE3, 0xE4, 0xDC));
    }

    fn assert_surface_everywhere_but_the_badge(buffer: &Buffer) {
        let surface = Some(Color::Rgb(0x15, 0x17, 0x19));
        for y in 0..23 {
            for x in 0..104 {
                if x > 0 && buffer[(x - 1, y)].symbol().width() == 2 {
                    continue;
                }
                let badge = y == 1 && (2..8).contains(&x);
                let expected = if badge {
                    Some(Color::Rgb(0xF3, 0xC2, 0x80))
                } else {
                    surface
                };
                assert_eq!(Some(buffer[(x, y)].bg), expected, "cell ({x}, {y})");
            }
        }
    }

    #[test]
    fn the_too_small_screen_names_both_sizes() {
        let state = AppState::new("build", false);
        let now = Instant::now();
        let buffer = drawn(&state, 36, 10, now);
        let text: Vec<String> = (0..10).map(|y| row(&buffer, y)).collect();
        let text = text.join("\n");
        for expected in [
            "ALBA",
            "Terminal too small",
            "Resize to at least 40×12",
            "Now 36×10",
        ] {
            assert!(text.contains(expected), "{expected:?} missing:\n{text}");
        }
    }

    #[test]
    fn a_copy_result_takes_over_the_key_hints_until_it_expires() {
        let now = Instant::now();
        let mut state = AppState::new("build", false);
        state.record_copy_result("copied (OSC 52)", now);
        assert!(row(&drawn(&state, 80, 24, now), 22).starts_with("  copied (OSC 52)   q quit"));
        let later = now + Duration::from_secs(3);
        assert!(row(&drawn(&state, 80, 24, later), 22).starts_with("  ↑↓ beam"));
    }

    #[test]
    fn long_durations_read_in_minutes() {
        assert_eq!(format_duration(Duration::from_secs(134)), "2m 14s");
        assert_eq!(format_duration(Duration::from_millis(9400)), "9.4s");
        assert_eq!(format_duration(Duration::from_millis(59_960)), "1m 0s");
        assert_eq!(format_duration(Duration::from_millis(119_600)), "2m 0s");
    }
}
