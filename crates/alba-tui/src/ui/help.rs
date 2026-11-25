//! The help page: `?` replaces the body with every binding of normal
//! mode, grouped, one binding per line. The page has no divider and no
//! beams pane; its title row reads `HELP / keyboard`, then the groups in
//! two columns, then a muted note that copy and graph modes list their
//! own keys in the key hints row.
//!
//! When the body is too narrow for two columns the groups stack in one,
//! in reading order; whatever still does not fit is cut at the bottom
//! and the right edge, never wrapped.
//!
//! [`COPY_KEYS`], [`GRAPH_KEYS`] and [`HELP_KEYS`] are the key hints of
//! those modes (`ui/keys.rs`), spelled out once here beside the page
//! that sends the reader to them.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::theme::{ColourMode, Token, style};

/// Copy mode's key hints.
pub(super) const COPY_KEYS: &str = "hjkl/arrows move · v anchor · y copy · Esc cancel";

/// Graph mode's key hints.
pub(super) const GRAPH_KEYS: &str = "↑↓←→ move · Enter open output · Esc back";

/// The help page's own key hints; `?` also closes it.
pub(super) const HELP_KEYS: &str = "Esc close help";

/// A group of bindings: its title, then `(key, action)` pairs.
type Group = (&'static str, &'static [(&'static str, &'static str)]);

const RUN: Group = (
    "RUN",
    &[
        ("r", "rerun"),
        ("f", "rerun without the cache"),
        ("c", "cancel the run"),
        ("t", "back to the session target"),
        ("w", "start or stop watching"),
    ],
);

const MOVE: Group = (
    "MOVE",
    &[
        ("↑↓  j k", "select a beam"),
        ("PgUp PgDn", "scroll the output"),
        ("G", "follow the output again"),
    ],
);

const FIND_AND_COPY: Group = (
    "FIND AND COPY",
    &[
        ("/", "search the output"),
        ("n  N", "next or previous match"),
        ("v", "select lines to copy"),
        ("y", "copy the selection"),
        ("s", "show stdout, stderr or both"),
    ],
);

const VIEWS: Group = (
    "VIEWS",
    &[
        ("g", "dependency graph"),
        ("?", "this help"),
        ("q", "quit"),
        ("Ctrl-C", "cancel, or quit when idle"),
    ],
);

const NOTE: &str = "Copy and graph modes list their own keys in the footer.";

/// The cells the key column takes, key and padding.
const KEY_COLUMN: usize = 12;
/// The groups' indent from the content column.
const INDENT: u16 = 2;
/// The least room between the two columns.
const COLUMN_GAP: u16 = 3;
/// Rows above the groups: the title row and a blank row.
const TITLE_ROWS: u16 = 2;

/// Draws the page into `body`, the frame's body rectangle.
pub fn draw(frame: &mut Frame, body: Rect, mode: ColourMode) {
    if body.width == 0 || body.height == 0 {
        return;
    }
    frame.render_widget(Paragraph::new(title(mode)), Rect { height: 1, ..body });
    if body.height <= TITLE_ROWS || body.width <= INDENT {
        return;
    }
    let area = Rect::new(
        body.x + INDENT,
        body.y + TITLE_ROWS,
        body.width - INDENT,
        body.height - TITLE_ROWS,
    );
    let left = column(&[RUN, MOVE], mode);
    let right = column(&[FIND_AND_COPY, VIEWS], mode);
    let note = Line::styled(NOTE, style(Token::TextMuted, mode));

    // Two columns when the left one and the gap fit before the middle of
    // the body and the right one fits after it.
    let middle = body.width / 2;
    let fits_two =
        INDENT + width(&left) + COLUMN_GAP <= middle && middle + width(&right) <= body.width;
    if fits_two {
        let rows = left.len().max(right.len()) as u16;
        let left_width = middle - INDENT;
        frame.render_widget(
            Paragraph::new(left),
            Rect {
                width: left_width,
                ..area
            },
        );
        frame.render_widget(
            Paragraph::new(right),
            Rect::new(body.x + middle, area.y, body.width - middle, area.height),
        );
        if rows < area.height {
            frame.render_widget(
                Paragraph::new(note),
                Rect::new(area.x, area.y + rows, area.width, 1),
            );
        }
    } else {
        let mut lines = left;
        lines.push(Line::default());
        lines.extend(right);
        lines.push(note);
        frame.render_widget(Paragraph::new(lines), area);
    }
}

/// `HELP / keyboard`: the head bold, the slash in `line`, the page name
/// muted.
fn title(mode: ColourMode) -> Line<'static> {
    Line::from(vec![
        Span::styled("HELP", style(Token::Text, mode).bold()),
        Span::styled(" / ", style(Token::Line, mode)),
        Span::styled("keyboard", style(Token::TextMuted, mode)),
    ])
}

/// `groups` one under the other, a blank line between two: the title
/// muted bold, then one line per binding, the key in `accent` bold in a
/// 12-cell column and the action in `text`.
fn column(groups: &[Group], mode: ColourMode) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, (title, bindings)) in groups.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::styled(*title, style(Token::TextMuted, mode).bold()));
        for (key, action) in *bindings {
            let pad = KEY_COLUMN.saturating_sub(key.width());
            lines.push(Line::from(vec![
                Span::styled(*key, style(Token::Accent, mode).bold()),
                Span::raw(" ".repeat(pad)),
                Span::styled(*action, style(Token::Text, mode)),
            ]));
        }
    }
    lines
}

fn width(lines: &[Line]) -> u16 {
    lines.iter().map(Line::width).max().unwrap_or(0) as u16
}

#[cfg(test)]
mod tests {
    use crate::state::{AppState, Mode};
    use alba_core::BeamId;
    use alba_engine::RunEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};
    use std::time::Instant;

    const ACCENT: Color = Color::Rgb(0xF3, 0xC2, 0x80);
    const MUTED: Color = Color::Rgb(0xA1, 0xA7, 0xA4);
    const TEXT: Color = Color::Rgb(0xE3, 0xE4, 0xDC);

    fn helping() -> AppState {
        let mut state =
            AppState::new("build", false).with_colour(super::super::theme::ColourMode::TrueColour);
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
        state.enter_help();
        assert_eq!(state.mode, Mode::Help);
        state
    }

    fn drawn(state: &AppState, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, state, Instant::now()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn screen(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| row(buffer, y))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The column of `needle` on row `y`, in cells.
    fn column(buffer: &Buffer, y: u16, needle: &str) -> Option<u16> {
        (0..buffer.area.width).find(|&x| {
            let rest: String = (x..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            rest.starts_with(needle)
        })
    }

    const BINDINGS: [(&str, &str); 17] = [
        ("r", "rerun"),
        ("f", "rerun without the cache"),
        ("c", "cancel the run"),
        ("t", "back to the session target"),
        ("w", "start or stop watching"),
        ("↑↓  j k", "select a beam"),
        ("PgUp PgDn", "scroll the output"),
        ("G", "follow the output again"),
        ("/", "search the output"),
        ("n  N", "next or previous match"),
        ("v", "select lines to copy"),
        ("y", "copy the selection"),
        ("s", "show stdout, stderr or both"),
        ("g", "dependency graph"),
        ("?", "this help"),
        ("q", "quit"),
        ("Ctrl-C", "cancel, or quit when idle"),
    ];

    /// Wide enough for two columns: every binding is one line, key in a
    /// 12-cell column (accent bold), action in `text`; the page replaces
    /// the body, so there is no beams pane, divider or junction.
    #[test]
    fn the_help_page_lists_every_binding_once() {
        let buffer = drawn(&helping(), 100, 24);
        let page = screen(&buffer);
        assert!(row(&buffer, 5).starts_with("  HELP / keyboard"), "{page}");
        assert_eq!(buffer[(2, 5)].fg, TEXT);
        assert!(buffer[(2, 5)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(9, 5)].fg, MUTED, "the page name");
        for (key, action) in BINDINGS {
            let entry = format!("{key}{}{action}", " ".repeat(12 - key.chars().count()));
            assert_eq!(page.matches(&entry).count(), 1, "{entry:?} once in\n{page}");
            let y = (5..19).find(|&y| row(&buffer, y).contains(&entry)).unwrap();
            let x = column(&buffer, y, &entry).unwrap();
            assert_eq!(buffer[(x, y)].fg, ACCENT, "{key:?}");
            assert!(buffer[(x, y)].modifier.contains(Modifier::BOLD), "{key:?}");
            assert_eq!(buffer[(x + 12, y)].fg, TEXT, "{action:?}");
        }
        // Two columns: RUN and FIND AND COPY open on the same row.
        let groups = row(&buffer, 7);
        assert!(
            groups.contains("RUN") && groups.contains("FIND AND COPY"),
            "{page}"
        );
        let x = column(&buffer, 7, "RUN").unwrap();
        assert_eq!(x, 4);
        assert_eq!(buffer[(x, 7)].fg, MUTED);
        assert!(buffer[(x, 7)].modifier.contains(Modifier::BOLD));
        assert!(row(&buffer, 14).contains("MOVE") && row(&buffer, 14).contains("VIEWS"));
        let last = "Copy and graph modes list their own keys in the footer.";
        let y = (5..21)
            .find(|&y| row(&buffer, y).contains(last))
            .expect(&page);
        assert_eq!(y, 19, "under the taller column\n{page}");
        assert_eq!(buffer[(column(&buffer, y, last).unwrap(), y)].fg, MUTED);
        assert_eq!(
            row(&buffer, 4),
            format!(" {} ", "─".repeat(98)),
            "no junction"
        );
        assert_eq!(
            row(&buffer, 21),
            format!(" {} ", "─".repeat(98)),
            "no junction"
        );
        assert!(!page.contains('│') && !page.contains("BEAMS"), "{page}");
        assert!(row(&buffer, 22).starts_with("  Esc close help"));
    }

    /// Too narrow for two columns: the groups stack in one column, in
    /// reading order, and whatever does not fit is cut at the bottom.
    #[test]
    fn a_narrow_page_stacks_the_groups_then_cuts() {
        let buffer = drawn(&helping(), 80, 30);
        let page = screen(&buffer);
        let rows: Vec<u16> = ["RUN", "MOVE", "FIND AND COPY", "VIEWS"]
            .iter()
            .map(|title| {
                (5..27)
                    .find(|&y| row(&buffer, y).trim() == *title)
                    .unwrap_or_else(|| panic!("{title} alone on a row\n{page}"))
            })
            .collect();
        assert!(rows.windows(2).all(|pair| pair[0] < pair[1]), "{page}");

        for (width, height) in [(40, 12), (40, 24), (60, 12), (85, 16)] {
            let small = drawn(&helping(), width, height);
            let page = screen(&small);
            assert!(row(&small, 5).contains("HELP / keyboard"), "{page}");
            assert!(row(&small, 7).contains("RUN"), "{page}");
            assert!(row(&small, height - 2).contains("Esc close help"), "{page}");
        }
    }
}
