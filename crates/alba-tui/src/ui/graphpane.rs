//! The graph view: the run's dependency DAG, laid out by `graph::layers`
//! and drawn as one row of `[glyph name]` nodes per layer — dependency
//! order top to bottom, since `graph::layers` puts a beam's own
//! dependencies in a strictly lower layer number than the beam itself —
//! with a connector row between consecutive layers linking each beam
//! down to its dependencies above.
//!
//! Every node's horizontal position comes from `graph::slot_center_fraction`,
//! the same function `GraphState::navigate` uses to judge "nearest by
//! horizontal position" when crossing layers — the one shared computation
//! that keeps this module's drawing and that module's hit-testing from
//! ever disagreeing about where a node sits, the way `copy::scroll_window`
//! keeps the log pane's scrolling and its own hit-testing in step.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::widgets::Paragraph;

use crate::graph::{self, GraphState};
use crate::state::AppState;

use super::tree::glyph_for;

pub fn draw(frame: &mut Frame, area: Rect, state: &AppState, graph_state: &GraphState) {
    let layers = graph::layers(state.beams.len(), &state.edges);
    if layers.is_empty() || area.height == 0 || area.width == 0 {
        return;
    }

    // One row per layer, one connector row between each consecutive
    // pair — laid out by hand rather than through `Layout::vertical`:
    // asked for more `Length(1)` rows than `area` is tall, it hands back
    // that many `Rect`s anyway, some of them past the area's own bottom
    // edge, rather than dropping the ones that do not fit. `row_rect`
    // is what actually stops a graph taller than the body at the
    // body's own edge.
    for (layer_index, nodes) in layers.iter().enumerate() {
        let Some(row_area) = row_rect(area, layer_index * 2) else {
            break; // the body is shorter than the graph
        };
        draw_node_row(frame, row_area, state, graph_state, nodes);
    }
    for boundary in 0..layers.len().saturating_sub(1) {
        let Some(row_area) = row_rect(area, boundary * 2 + 1) else {
            break;
        };
        draw_connectors(frame, row_area, &layers, boundary, state);
    }
}

/// The one-row-tall rectangle `row_index` rows below `area`'s own top,
/// spanning its full width — or `None` once that row would fall at or
/// past `area`'s bottom edge, which is what keeps a graph with more
/// layers than the body has room for from ever asking `ratatui` to draw
/// outside the buffer it was given.
fn row_rect(area: Rect, row_index: usize) -> Option<Rect> {
    let offset = u16::try_from(row_index).ok()?;
    let y = area.y.checked_add(offset)?;
    if y >= area.y + area.height {
        return None;
    }
    Some(Rect::new(area.x, y, area.width, 1))
}

fn draw_node_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    graph_state: &GraphState,
    nodes: &[usize],
) {
    let count = nodes.len();
    for (position, &beam) in nodes.iter().enumerate() {
        let fraction = graph::slot_center_fraction(position, count);
        let row = &state.beams[beam];
        let label = format!("[{} {}]", glyph_for(&row.state), row.id);
        let style = node_style(beam, graph_state.focused);
        draw_label(frame, area, fraction, &label, style);
    }
}

/// The focused node reversed, every other node plain — pulled out of
/// `draw_node_row` so it can be pinned by a unit test directly.
/// `TestBackend::to_string()` drops styles entirely, so the reversed span
/// itself would pass every render snapshot silently whether or not this
/// ever actually fired; this is what `ui/logpane.rs`'s `styled_line` does
/// for the very same reason.
fn node_style(beam: usize, focused: usize) -> Style {
    if beam == focused {
        Style::default().reversed()
    } else {
        Style::default()
    }
}

/// Draws `label` centred on `fraction` of `area`'s width, clamped so a
/// label wider than the room it lands in (an over-wide layer, or a very
/// long beam id) never asks for a rectangle outside `area` itself — hand
/// -computed positions do not clip themselves the way `ratatui`'s own
/// widgets clip *within* the rectangle they are given.
fn draw_label(frame: &mut Frame, area: Rect, fraction: f64, label: &str, style: Style) {
    let center_x = fraction_to_x(area, fraction);
    let width = label.chars().count() as u16;
    let start_x = center_x.saturating_sub(width / 2).max(area.x);
    let right_edge = area.x + area.width;
    if start_x >= right_edge {
        return;
    }
    let rect_width = width.min(right_edge - start_x);
    frame.render_widget(
        Paragraph::new(label.to_string()).style(style),
        Rect::new(start_x, area.y, rect_width, 1),
    );
}

/// Every edge whose dependency's layer is at or above `boundary` and
/// whose beam's layer is below it passes through this connector row — an
/// edge spanning several layers passes straight through every
/// intermediate row at its dependency's own column, and only bends
/// toward its beam's column on the row immediately above that beam.
fn draw_connectors(
    frame: &mut Frame,
    area: Rect,
    layers: &[Vec<usize>],
    boundary: usize,
    state: &AppState,
) {
    for &(beam, dependency) in &state.edges {
        let Some(dependency_layer) = layer_of(layers, dependency) else {
            continue;
        };
        let Some(beam_layer) = layer_of(layers, beam) else {
            continue;
        };
        if dependency_layer > boundary || beam_layer <= boundary {
            continue; // this edge does not cross this particular row
        }
        let top_x = node_x(area, layers, dependency_layer, dependency);
        let bottom_x = if beam_layer == boundary + 1 {
            node_x(area, layers, beam_layer, beam)
        } else {
            top_x // still passing through: no bend until the beam's own row
        };
        draw_segment(frame, area, top_x, bottom_x);
    }
}

fn layer_of(layers: &[Vec<usize>], beam: usize) -> Option<usize> {
    layers.iter().position(|nodes| nodes.contains(&beam))
}

fn node_x(area: Rect, layers: &[Vec<usize>], layer_index: usize, beam: usize) -> u16 {
    let nodes = &layers[layer_index];
    // `beam` always belongs to `layers[layer_index]` here: both callers
    // (`draw_connectors`) look it up via `layer_of` just before.
    let position = nodes.iter().position(|&node| node == beam).unwrap_or(0);
    let fraction = graph::slot_center_fraction(position, nodes.len());
    fraction_to_x(area, fraction)
}

fn draw_segment(frame: &mut Frame, area: Rect, top_x: u16, bottom_x: u16) {
    use std::cmp::Ordering;
    match top_x.cmp(&bottom_x) {
        Ordering::Equal => set_char(frame, area, top_x, '│'),
        Ordering::Less => {
            set_char(frame, area, top_x, '╰');
            for x in (top_x + 1)..bottom_x {
                set_char(frame, area, x, '─');
            }
            set_char(frame, area, bottom_x, '╮');
        }
        Ordering::Greater => {
            set_char(frame, area, top_x, '╯');
            for x in (bottom_x + 1)..top_x {
                set_char(frame, area, x, '─');
            }
            set_char(frame, area, bottom_x, '╭');
        }
    }
}

fn set_char(frame: &mut Frame, area: Rect, x: u16, character: char) {
    if x < area.x || x >= area.x + area.width {
        return;
    }
    frame
        .buffer_mut()
        .set_string(x, area.y, character.to_string(), Style::default());
}

/// Turns a `slot_center_fraction` into an actual column inside `area`,
/// clamped to `area`'s own last column — the one place both the node row
/// and the connector row convert "how far across" into "which column",
/// so they can never drift apart over a rounding difference.
fn fraction_to_x(area: Rect, fraction: f64) -> u16 {
    if area.width == 0 {
        return area.x;
    }
    let offset = (fraction * area.width as f64).round() as u16;
    (area.x + offset).min(area.x + area.width - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::RunEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    fn state_with(beams: &[&str], edges: &[(&str, &str)]) -> AppState {
        let mut state = AppState::new("t", false);
        state.apply(
            &RunEvent::RunStarted {
                target: BeamId("t".to_string()),
                beams: beams.iter().map(|name| BeamId(name.to_string())).collect(),
                edges: edges
                    .iter()
                    .map(|(beam, dep)| (BeamId(beam.to_string()), BeamId(dep.to_string())))
                    .collect(),
            },
            Instant::now(),
        );
        state
    }

    /// Draws `state` at `width`x`height` and returns the rendered text —
    /// a failed `terminal.draw` (a panic anywhere in `draw`'s arithmetic)
    /// fails these tests loudly, which is the point: hand-computed
    /// positions do not clip themselves the way `ratatui`'s own widgets
    /// clip within the rectangle they are given, so every guard case
    /// below only has to prove nothing panics.
    fn draw_at(state: &AppState, graph_state: &GraphState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), state, graph_state))
            .unwrap();
        terminal.backend().to_string()
    }

    /// The smallest possible graph: one beam, no edges, so there is
    /// nothing above it to connect to and no connector row at all.
    #[test]
    fn a_single_beam_with_no_edges_draws_without_a_connector() {
        let state = state_with(&["solo"], &[]);
        let rendered = draw_at(&state, &GraphState::new(0), 80, 24);
        assert!(rendered.contains("solo"), "the one node still draws");
    }

    /// A layer with more nodes than the terminal is wide enough to fit
    /// them all legibly — every node's column still comes from
    /// `slot_center_fraction`, which never exceeds the area's own width,
    /// so this must not panic even though nodes overlap on screen.
    #[test]
    fn a_layer_wider_than_the_terminal_does_not_panic() {
        let names: Vec<String> = (0..40).map(|index| format!("beam{index}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let state = state_with(&refs, &[]);
        let rendered = draw_at(&state, &GraphState::new(0), 30, 10);
        assert!(!rendered.trim().is_empty());
    }

    /// A beam id far wider than the terminal must have its label clipped
    /// by `ratatui`'s own `Paragraph` rendering rather than asking for a
    /// rectangle that reaches outside the render area.
    #[test]
    fn a_very_long_beam_id_does_not_panic() {
        let long_name = "x".repeat(200);
        let state = state_with(&[long_name.as_str()], &[]);
        let rendered = draw_at(&state, &GraphState::new(0), 80, 24);
        assert!(rendered.contains('x'), "the label still draws, clipped");
    }

    /// A chain deep enough that its layers (and their connector rows)
    /// outnumber the body's own height: `row_rect` stops handing back
    /// rows once they would fall past the body's own bottom edge, rather
    /// than asking `ratatui` to draw outside the buffer it was given.
    #[test]
    fn a_graph_taller_than_the_body_does_not_panic() {
        let names: Vec<String> = (0..30).map(|index| format!("beam{index}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let edges: Vec<(&str, &str)> = (1..refs.len()).map(|i| (refs[i], refs[i - 1])).collect();
        let state = state_with(&refs, &edges);
        let rendered = draw_at(&state, &GraphState::new(0), 80, 10);
        assert!(rendered.contains("beam0"), "the top layer still fits");
    }

    /// `TestBackend::to_string()` drops styles, so the render snapshots
    /// cannot tell a focused node from any other — this is what actually
    /// pins the reversed style, the same reason `ui/logpane.rs` has its
    /// own unit test over `styled_line` rather than trusting a snapshot.
    #[test]
    fn node_style_reverses_only_the_focused_beam() {
        assert_eq!(node_style(2, 2), Style::default().reversed());
        assert_eq!(node_style(1, 2), Style::default());
    }

    /// A single-node layer centres in the middle of its row regardless
    /// of the row's own width.
    #[test]
    fn fraction_to_x_centres_a_single_node() {
        let area = Rect::new(0, 0, 40, 1);
        assert_eq!(fraction_to_x(area, graph::slot_center_fraction(0, 1)), 20);
    }

    /// Never asks for a column past the area's own last one, even at the
    /// fraction closest to 1.0 — the guard against a layer wider than the
    /// terminal (many nodes) drifting the rightmost one off the edge.
    #[test]
    fn fraction_to_x_never_reaches_past_the_areas_last_column() {
        let area = Rect::new(5, 0, 10, 1);
        let fraction = graph::slot_center_fraction(99, 100);
        let x = fraction_to_x(area, fraction);
        assert!(x < area.x + area.width, "x={x} area={area:?}");
    }

    /// The rendered row's own cells, indexed exactly like `draw_segment`'s
    /// `top_x`/`bottom_x` columns. `TestBackend::to_string()` wraps each
    /// row in a quoted, newline-terminated line for readability (see any
    /// committed `.snap` file) — stripped back off here so a cell index
    /// into this `Vec` means the same column `draw_segment` was asked to
    /// draw at, not one shifted by that framing.
    fn draw_segment_render(top_x: u16, bottom_x: u16) -> Vec<char> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(10, 1)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_segment(frame, area, top_x, bottom_x);
            })
            .unwrap();
        terminal
            .backend()
            .to_string()
            .trim_end_matches('\n')
            .trim_matches('"')
            .chars()
            .collect()
    }

    /// Aligned columns need nothing more than a plain vertical bar.
    #[test]
    fn draw_segment_draws_a_bar_when_the_columns_align() {
        let cells = draw_segment_render(3, 3);
        assert_eq!(cells[3], '│');
    }

    /// Bending right (the dependency sits left of the beam below it):
    /// the top end sends its line rightward (`╰`, up + right), the
    /// bottom end receives it from the left (`╮`, left + down), and
    /// every column strictly between is filled with a dash.
    #[test]
    fn draw_segment_bends_right_with_matching_corners() {
        let cells = draw_segment_render(1, 7);
        assert_eq!(cells[1], '╰', "left end: up + right");
        assert_eq!(cells[7], '╮', "right end: left + down");
        for (x, &cell) in cells.iter().enumerate().take(7).skip(2) {
            assert_eq!(cell, '─', "filled between at {x}");
        }
    }

    /// Bending left is the mirror image, with the corners swapped.
    #[test]
    fn draw_segment_bends_left_with_matching_corners() {
        let cells = draw_segment_render(7, 1);
        assert_eq!(cells[7], '╯', "right end: up + left");
        assert_eq!(cells[1], '╭', "left end: down + right");
        for (x, &cell) in cells.iter().enumerate().take(7).skip(2) {
            assert_eq!(cell, '─', "filled between at {x}");
        }
    }
}
