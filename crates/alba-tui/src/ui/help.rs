//! The help overlay: a centred bordered box listing every binding the
//! interactive modes actually implement, drawn over the rest of the body
//! (the tree and log panes, already painted by `draw` before this runs)
//! dimmed rather than hidden — the reader is still looking at their run,
//! just through a reference card.
//!
//! [`keymap_lines`] is transcribed by hand from `input.rs`'s keymap and
//! `state.rs`'s modal handlers (`handle_search_key`, `handle_copy_key`,
//! `handle_graph_key`) rather than generated from them, so it is only as
//! honest as whoever last read those two files kept it — the render
//! snapshot in `tests/render.rs` is what actually catches drift, but only
//! for a reviewer who checks each line against the code it describes.
//!
//! Copy's, Graph's, and this overlay's own closing keys are *also* what
//! `ui/mod.rs`'s `bottom_bar` shows for those same modes, character for
//! character — [`COPY_KEYS`], [`GRAPH_KEYS`], and [`HELP_KEYS`] are the
//! one place that text is spelled out, so the two screens cannot drift
//! apart the way a hand-copied second literal eventually would.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

/// Copy mode's keymap — shared verbatim with `ui/mod.rs`'s `bottom_bar`
/// (its `Mode::Copy` arm reads this same constant), rather than the two
/// screens each spelling it out on their own.
pub(super) const COPY_KEYS: &str = "hjkl/arrows move · v anchor · y copy · Esc cancel";

/// Graph mode's keymap — no `hjkl` here, unlike Copy; shared verbatim
/// with `bottom_bar`'s `Mode::Graph` arm the same way `COPY_KEYS` is.
pub(super) const GRAPH_KEYS: &str = "↑↓←→ move · Enter select · Esc back";

/// How to close the overlay itself — shared verbatim with `bottom_bar`'s
/// `Mode::Help` arm.
pub(super) const HELP_KEYS: &str = "Esc or ? close";

/// The full keymap, one line per group of related bindings, blank lines
/// separating the four groups (Normal / Search / Copy / Graph) the
/// interactive tasks built, plus this overlay's own closing keys.
/// Phrased to match the wording the bottom bar already uses for the
/// bindings it has room to show (`ui/mod.rs`'s `bottom_bar`), so the
/// overlay never contradicts what the reader has already half-learned
/// from the corner of their eye — Copy, Graph, and Help go further and
/// share the exact same constant (see the module doc comment).
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
fn keymap_lines() -> Vec<String> {
    vec![
        "Normal".to_string(),
        "  q quit · r rerun · f force (bypass cache) · c cancel · w watch".to_string(),
        "  j/k, ↑/↓ move · n next match · N previous match · G tail".to_string(),
        "  / search · v copy · g graph · ? help · Ctrl-C cancel/quit (any mode)".to_string(),
        String::new(),
        "Search (composing a query)".to_string(),
        "  typing edits the query · Backspace erase · Enter commit · Esc cancel".to_string(),
        String::new(),
        "Copy".to_string(),
        format!("  {COPY_KEYS}"),
        String::new(),
        "Graph".to_string(),
        format!("  {GRAPH_KEYS}"),
        String::new(),
        "Help".to_string(),
        format!("  {HELP_KEYS}"),
    ]
}

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

    let content = keymap_lines();
    let popup = centered_box(area, box_width(&content), box_height(&content));
    // `Clear` first: the popup's own content must read clearly against
    // a plain background, not the dimmed characters now sitting under it.
    frame.render_widget(Clear, popup);
    let lines: Vec<Line> = content
        .iter()
        .map(|line| Line::from(line.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" help ")),
        popup,
    );
}

fn box_width(lines: &[String]) -> u16 {
    lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0) as u16
        + 4
}

fn box_height(lines: &[String]) -> u16 {
    lines.len() as u16 + 2
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
        let lines = keymap_lines();
        let popup = centered_box(area, box_width(&lines), box_height(&lines));
        assert!(popup.width <= area.width);
        assert!(popup.height <= area.height);
    }

    /// `keymap_lines` must build its Copy/Graph/Help entries from
    /// `COPY_KEYS`/`GRAPH_KEYS`/`HELP_KEYS`, not a hand-copied literal of
    /// its own — `ui/mod.rs`'s own covering test
    /// (`copy_graph_and_help_bars_match_their_shared_keymap_constants`)
    /// checks the same constants from `bottom_bar`'s side; between the
    /// two, neither file can silently drift from the other without one
    /// of these breaking.
    #[test]
    fn keymap_lines_build_copy_graph_and_help_from_the_shared_constants() {
        let lines = keymap_lines();
        assert!(lines.contains(&format!("  {COPY_KEYS}")));
        assert!(lines.contains(&format!("  {GRAPH_KEYS}")));
        assert!(lines.contains(&format!("  {HELP_KEYS}")));
    }
}
