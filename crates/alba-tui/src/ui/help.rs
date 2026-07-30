//! The help overlay: a centred bordered box listing every binding the
//! interactive modes actually implement, drawn over the rest of the body
//! (the tree and log panes, already painted by `draw` before this runs)
//! dimmed rather than hidden — the reader is still looking at their run,
//! just through a reference card.
//!
//! [`KEYMAP`] is transcribed by hand from `input.rs`'s keymap and
//! `state.rs`'s modal handlers (`handle_search_key`, `handle_copy_key`,
//! `handle_graph_key`) rather than generated from them, so it is only as
//! honest as whoever last read those two files kept it — the render
//! snapshot in `tests/render.rs` is what actually catches drift, but only
//! for a reviewer who checks each line against the code it describes.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

/// The full keymap, one line per group of related bindings, blank lines
/// separating the four groups (Normal / Search / Copy / Graph) the
/// interactive tasks built. Phrased to match the wording the bottom bar
/// already uses for the bindings it has room to show (`ui/mod.rs`'s
/// `bottom_bar`), so the overlay never contradicts what the reader has
/// already half-learned from the corner of their eye.
///
/// A few entries are worth flagging for whoever next edits this list:
/// - `n`/`N` are Normal-mode bindings on the *committed* search, not
///   something typed while composing one — while a query is being typed
///   every character, `n`/`N` included, just edits it.
/// - `f` reruns the same as `r`, but bypassing the cache.
/// - Graph mode only answers the four arrow keys, not `hjkl` — copy mode
///   is the one that answers both.
/// - `Ctrl-C` fires the same in every mode (cancel if a run is in
///   flight, else quit) — listed once, under Normal, rather than
///   repeated in each group.
const KEYMAP: &[&str] = &[
    "Normal",
    "  q quit · r rerun · f force (bypass cache) · c cancel · w watch",
    "  j/k, \u{2191}/\u{2193} move · n next match · N previous match · G tail",
    "  / search · v copy · g graph · ? help · Ctrl-C cancel/quit (any mode)",
    "",
    "Search (composing a query)",
    "  typing edits the query · Backspace erase · Enter commit · Esc cancel",
    "",
    "Copy",
    "  hjkl/arrows move · v anchor · y copy · Esc cancel",
    "",
    "Graph",
    "  \u{2191}\u{2193}\u{2190}\u{2192} move · Enter select · Esc back",
];

/// Draws the overlay into `area` — the same body rectangle `graph::draw`
/// replaces entirely, here instead dimmed and given a centred box on top.
/// A no-op on a degenerate (zero-sized) area; `draw` only ever reaches
/// this once the too-small-screen floor has already been cleared, but
/// nothing here needs that guarantee to hold.
pub fn draw(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Dim what is already drawn there (the tree and log panes) rather
    // than blanking it: the reader is consulting a reference card, not
    // leaving the screen they were just reading.
    frame
        .buffer_mut()
        .set_style(area, Style::default().add_modifier(Modifier::DIM));

    let popup = centered_box(area, box_width(), box_height());
    // `Clear` first: the popup's own content must read clearly against
    // a plain background, not the dimmed characters now sitting under it.
    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = KEYMAP.iter().map(|line| Line::from(*line)).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" help ")),
        popup,
    );
}

fn box_width() -> u16 {
    KEYMAP
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0) as u16
        + 4
}

fn box_height() -> u16 {
    KEYMAP.len() as u16 + 2
}

/// `width`x`height` centred inside `area`, clamped to `area`'s own
/// bounds — a body too small to hold the box at its natural size gets
/// the box shrunk to fit rather than a rectangle asked for outside
/// `area`; `ratatui`'s own `Paragraph` then clips whatever content no
/// longer fits, the same way `ui/graphpane.rs`'s node labels clip
/// against a layer wider than the terminal.
fn centered_box(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn draw_at(width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, frame.area())).unwrap();
        terminal.backend().to_string()
    }

    /// The box centres with room to spare in a generously sized body,
    /// and every group's own line is on screen.
    #[test]
    fn the_box_centres_and_shows_every_group() {
        let rendered = draw_at(80, 22);
        for group in ["Normal", "Search", "Copy", "Graph"] {
            assert!(rendered.contains(group), "missing group {group:?}");
        }
        assert!(rendered.contains("help"));
    }

    /// A body too small to hold the box at its natural size must not
    /// panic — `centered_box` shrinks to fit instead of asking `ratatui`
    /// to draw outside `area`.
    #[test]
    fn a_body_too_small_for_the_box_does_not_panic() {
        let rendered = draw_at(10, 4);
        assert!(!rendered.trim().is_empty());
    }

    /// A degenerate, zero-sized body draws nothing rather than dividing
    /// by (or subtracting into) a size that was never there. A
    /// `TestBackend` itself cannot be zero-sized (`ratatui` panics
    /// constructing one), so this drives `draw` at a real terminal's
    /// zero-sized *sub*-rectangle instead — the only way this area can
    /// actually arise, since `ui::draw` never calls here below its own
    /// too-small-screen floor.
    #[test]
    fn a_zero_sized_body_does_not_panic() {
        let mut terminal = Terminal::new(TestBackend::new(10, 10)).unwrap();
        terminal
            .draw(|frame| draw(frame, Rect::new(0, 0, 0, 0)))
            .unwrap();
    }

    /// The box never exceeds the area it is asked to centre inside,
    /// whatever its natural size would otherwise be.
    #[test]
    fn centered_box_never_exceeds_the_area() {
        let area = Rect::new(0, 0, 6, 3);
        let popup = centered_box(area, box_width(), box_height());
        assert!(popup.width <= area.width);
        assert!(popup.height <= area.height);
    }
}
