//! The graph view, in the right pane beside the beams: a title row,
//! the run's dependency DAG laid out by `graph::layers` as one row of
//! `[ glyph name ]` nodes per layer (dependencies above the beams that
//! need them), a connector row between consecutive layers, then
//! `COMMANDS` for the focused beam. The edges on the focused beam's path
//! to the targets are drawn in `accent`, the others in `line`. When the
//! pane is too short, `COMMANDS` goes first, then the DAG scrolls to
//! keep the focused node on screen.
//!
//! Every node's horizontal position comes from `graph::slot_center_fraction`,
//! the same function `GraphState::navigate` uses to judge "nearest by
//! horizontal position" when crossing layers: the one shared computation
//! that keeps this module's drawing and that module's hit-testing from
//! ever disagreeing about where a node sits.

use std::collections::HashSet;
use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use crate::graph::{self, GraphState};
use crate::state::{AppState, BeamState};

use super::beams::{self, fit_name, glyph_for, selection_style};
use super::theme::{ColourMode, Token, status_token, style};

/// The title row and the blank row under it.
const ROWS_ABOVE_DAG: u16 = 2;

/// The rows `COMMANDS` needs under the DAG (a blank row, its header and
/// at least three lines); with fewer it is dropped before the DAG ever
/// scrolls.
const COMMANDS_MIN_ROWS: usize = 5;

/// Draws the graph into `pane`, the right pane: the title row, the DAG,
/// then `COMMANDS` for the focused beam when there is room for it. Like
/// the output pane, the content starts one cell past the divider.
pub fn draw(frame: &mut Frame, pane: Rect, state: &AppState, graph_state: &GraphState) {
    if pane.width < 2 || pane.height == 0 {
        return;
    }
    let area = Rect::new(pane.x + 1, pane.y, pane.width - 1, pane.height);
    let layers = graph::layers(state.beams.len(), &state.edges);
    frame.render_widget(
        Paragraph::new(title_line(state, layers.len(), area.width as usize)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if layers.is_empty() {
        return;
    }

    let room = area.height.saturating_sub(ROWS_ABOVE_DAG) as usize;
    let dag_rows = layers.len() * 2 - 1;
    let focused_row = layer_of(&layers, graph_state.focused).map_or(0, node_row);
    let visible = if dag_rows <= room {
        0..dag_rows
    } else {
        beams::window(focused_row, dag_rows, room)
    };
    let dag = Rect::new(
        area.x,
        area.y + ROWS_ABOVE_DAG.min(area.height),
        area.width,
        room as u16,
    );
    let path = graph::focused_path(&state.edges, graph_state.focused, &state.targets);

    for (layer_index, nodes) in layers.iter().enumerate() {
        if let Some(row_area) = row_rect(dag, &visible, node_row(layer_index)) {
            draw_node_row(frame, row_area, state, graph_state, nodes);
        }
    }
    for boundary in 0..layers.len() - 1 {
        if let Some(row_area) = row_rect(dag, &visible, connector_row(boundary)) {
            draw_connectors(frame, row_area, &layers, boundary, state, &path);
        }
    }

    if dag_rows + COMMANDS_MIN_ROWS <= room {
        let below = dag_rows as u16 + 1; // a blank row under the DAG
        let section = Rect::new(dag.x, dag.y + below, dag.width, dag.height - below);
        draw_commands(frame, section, state, graph_state.focused);
    }
}

/// The DAG's own row of layer `layer_index`'s nodes: one node row per
/// layer, one connector row between consecutive layers.
fn node_row(layer_index: usize) -> usize {
    layer_index * 2
}

/// The DAG's own row of the connectors between layer `boundary` and the
/// next one.
fn connector_row(boundary: usize) -> usize {
    boundary * 2 + 1
}

/// The screen row of the DAG's row `index` inside `dag`, or `None` when
/// the scroll window `visible` leaves it off screen.
fn row_rect(dag: Rect, visible: &Range<usize>, index: usize) -> Option<Rect> {
    if !visible.contains(&index) {
        return None;
    }
    let offset = u16::try_from(index - visible.start).ok()?;
    Some(Rect::new(dag.x, dag.y + offset, dag.width, 1))
}

/// `GRAPH / target`, with `N beams · M layers` muted on the right. The
/// counter is dropped before the target is cut.
fn title_line(state: &AppState, layer_count: usize, width: usize) -> Line<'static> {
    let mode = state.colour;
    let counter = format!(
        "{} · {}",
        counted(state.beams.len(), "beam"),
        counted(layer_count, "layer")
    );
    let head = "GRAPH";
    let separator = " / ";
    let target = state.target.as_str();
    let full = head.width() + separator.width() + target.width();
    let room = if full + 1 + counter.width() <= width {
        width - counter.width() - 1
    } else {
        width
    };

    let mut spans = Vec::new();
    if head.width() <= room {
        spans.push(Span::styled(head, style(Token::Text, mode).bold()));
        let lead = head.width() + separator.width();
        if !target.is_empty() && room > lead {
            spans.push(Span::styled(separator, style(Token::Line, mode)));
            spans.push(Span::styled(
                fit_name(target, room - lead),
                style(Token::Accent, mode).bold(),
            ));
        }
    }
    if room < width {
        let used: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::raw(" ".repeat(width - used - counter.width())));
        spans.push(Span::styled(counter, style(Token::TextMuted, mode)));
    }
    Line::from(spans)
}

/// `1 beam`, `4 beams`.
fn counted(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// `COMMANDS / beam` with `NEEDED BY` and the location, the description
/// muted, then each command as written, one `$ command` line apiece,
/// as many as `area` holds.
fn draw_commands(frame: &mut Frame, area: Rect, state: &AppState, focused: usize) {
    let width = area.width as usize;
    let mode = state.colour;
    let Some(beam) = state.beams.get(focused) else {
        return;
    };
    let details = state.details.get(&beam.id);
    let mut lines = vec![commands_header(state, focused, width)];
    if let Some(description) = details.and_then(|details| details.description.as_deref()) {
        lines.push(Line::styled(
            fit_name(description, width),
            style(Token::TextMuted, mode),
        ));
    }
    for command in details
        .map(|details| details.commands.as_slice())
        .unwrap_or_default()
    {
        lines.push(Line::from(vec![
            Span::styled("$ ", style(Token::Accent, mode).bold()),
            Span::styled(
                fit_name(command, width.saturating_sub(2)),
                style(Token::Text, mode).bold(),
            ),
        ]));
    }
    for (offset, line) in lines.into_iter().take(area.height as usize).enumerate() {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y + offset as u16, area.width, 1),
        );
    }
}

/// The section's header: `COMMANDS / beam`, then `NEEDED BY` and the
/// beams that depend on it directly, the location on the right. When it
/// does not all fit, the dependents are cut first, then the beam's name.
fn commands_header(state: &AppState, focused: usize, width: usize) -> Line<'static> {
    let mode = state.colour;
    let beam = state.beams[focused].id.as_str();
    let location = state
        .details
        .get(beam)
        .and_then(|details| details.location.as_deref())
        .map(|location| fit_name(location, width))
        .unwrap_or_default();
    let room = if location.is_empty() {
        width
    } else {
        width.saturating_sub(location.width() + 1)
    };

    let head = "COMMANDS";
    let separator = " / ";
    let mut spans = Vec::new();
    if head.width() <= room {
        spans.push(Span::styled(head, style(Token::Text, mode).bold()));
        let lead = head.width() + separator.width();
        if room > lead {
            spans.push(Span::styled(separator, style(Token::Line, mode)));
            spans.push(Span::styled(
                fit_name(beam, room - lead),
                style(Token::Accent, mode).bold(),
            ));
        }
    }

    let needed_by = dependents(state, focused);
    let used: usize = spans.iter().map(Span::width).sum();
    let label = "    NEEDED BY ";
    if !needed_by.is_empty() && used + label.width() < room {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            "NEEDED BY",
            style(Token::TextMuted, mode).bold(),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            fit_name(&needed_by, room - used - label.width()),
            style(Token::Text, mode),
        ));
    }
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + location.width())),
    ));
    spans.push(Span::styled(location, style(Token::TextMuted, mode)));
    Line::from(spans)
}

/// The beams that depend on `focused` directly, in beam order, joined
/// with `, `.
fn dependents(state: &AppState, focused: usize) -> String {
    let mut beams: Vec<usize> = state
        .edges
        .iter()
        .filter(|&&(_, dependency)| dependency == focused)
        .map(|&(beam, _)| beam)
        .collect();
    beams.sort_unstable();
    beams.dedup();
    beams
        .iter()
        .map(|&beam| state.beams[beam].id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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
        let label = format!("[ {} {} ]", glyph_for(&row.state), row.id);
        let style = node_style(&row.state, beam, graph_state.focused, state.colour);
        draw_label(frame, area, fraction, &label, style);
    }
}

/// Every node is bold in its status colour (pending ones muted); the
/// focused one is the selection bar, as the selected beam's row is.
/// `TestBackend::to_string()` drops styles, so a unit test pins this.
fn node_style(state: &BeamState, beam: usize, focused: usize, colour: ColourMode) -> Style {
    if beam == focused {
        selection_style(colour)
    } else {
        style(status_token(state), colour).bold()
    }
}

/// Draws `label` centred on `fraction` of `area`'s width, clamped so a
/// label wider than the room it lands in (an over-wide layer, or a very
/// long beam id) never asks for a rectangle outside `area` itself — hand
/// -computed positions do not clip themselves the way `ratatui`'s own
/// widgets clip *within* the rectangle they are given.
fn draw_label(frame: &mut Frame, area: Rect, fraction: f64, label: &str, style: Style) {
    let center_x = fraction_to_x(area, fraction);
    let width = label.width() as u16;
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
/// whose beam's layer is below it passes through this connector row: an
/// edge spanning several layers passes straight through every
/// intermediate row at its dependency's own column, and only bends
/// toward its beam's column on the row immediately above that beam.
/// Edges on the focused path are drawn in `accent` after the others in
/// `line`, so where two share a cell the path wins.
fn draw_connectors(
    frame: &mut Frame,
    area: Rect,
    layers: &[Vec<usize>],
    boundary: usize,
    state: &AppState,
    path: &HashSet<(usize, usize)>,
) {
    let (on_path, off_path): (Vec<_>, Vec<_>) =
        state.edges.iter().partition(|edge| path.contains(edge));
    let passes = [
        (off_path, style(Token::Line, state.colour)),
        (on_path, style(Token::Accent, state.colour)),
    ];
    for (edges, edge_style) in passes {
        for &(beam, dependency) in edges {
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
            draw_segment(frame, area, top_x, bottom_x, edge_style);
        }
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

fn draw_segment(frame: &mut Frame, area: Rect, top_x: u16, bottom_x: u16, edge_style: Style) {
    use std::cmp::Ordering;
    match top_x.cmp(&bottom_x) {
        Ordering::Equal => set_char(frame, area, top_x, '│', edge_style),
        Ordering::Less => {
            set_char(frame, area, top_x, '╰', edge_style);
            for x in (top_x + 1)..bottom_x {
                set_char(frame, area, x, '─', edge_style);
            }
            set_char(frame, area, bottom_x, '╮', edge_style);
        }
        Ordering::Greater => {
            set_char(frame, area, top_x, '╯', edge_style);
            for x in (bottom_x + 1)..top_x {
                set_char(frame, area, x, '─', edge_style);
            }
            set_char(frame, area, bottom_x, '╭', edge_style);
        }
    }
}

fn set_char(frame: &mut Frame, area: Rect, x: u16, character: char, edge_style: Style) {
    if x < area.x || x >= area.x + area.width {
        return;
    }
    frame
        .buffer_mut()
        .set_string(x, area.y, character.to_string(), edge_style);
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
    use alba_engine::{BeamDetails, ExecutorLabel, RunEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier};
    use std::time::Instant;

    fn state_with(beams: &[&str], edges: &[(&str, &str)]) -> AppState {
        run(beams, edges, &["t"], Vec::new())
    }

    fn run(
        beams: &[&str],
        edges: &[(&str, &str)],
        targets: &[&str],
        details: Vec<BeamDetails>,
    ) -> AppState {
        let id = |name: &str| BeamId(name.to_string());
        let mut state = AppState::new(targets.join(", ").as_str(), false);
        state.apply(
            &RunEvent::RunStarted {
                targets: targets.iter().map(|name| id(name)).collect(),
                affected_by: None,
                beams: beams.iter().map(|name| id(name)).collect(),
                edges: edges
                    .iter()
                    .map(|(beam, dep)| (id(beam), id(dep)))
                    .collect(),
                details,
            },
            Instant::now(),
        );
        state.with_colour(ColourMode::TrueColour)
    }

    fn details(name: &str, commands: &[&str]) -> BeamDetails {
        BeamDetails {
            id: BeamId(name.to_string()),
            description: Some("Compile the workspace".to_string()),
            location: Some("Beamfile:12".to_string()),
            executor: ExecutorLabel {
                kind: "shell".to_string(),
                detail: None,
            },
            commands: commands.iter().map(|command| command.to_string()).collect(),
        }
    }

    /// `codegen` under `build` and `lint`, both under the target `test`.
    fn diamond(details: Vec<BeamDetails>) -> AppState {
        run(
            &["codegen", "build", "lint", "test"],
            &[
                ("build", "codegen"),
                ("lint", "codegen"),
                ("test", "build"),
                ("test", "lint"),
            ],
            &["test"],
            details,
        )
    }

    fn buffer_at(state: &AppState, focused: usize, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), state, &GraphState::new(focused)))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height).map(|y| row(buffer, y)).collect()
    }

    const ACCENT: Color = Color::Rgb(0xF3, 0xC2, 0x80);
    const LINE: Color = Color::Rgb(0x62, 0x6A, 0x67);

    /// Focused on `build`: its edge to `test` is the path, in `accent`;
    /// `lint`'s edge to `test` and both edges from `codegen` stay `line`.
    /// The focused node itself is `on-accent` on `accent`.
    #[test]
    fn the_focused_path_is_accent() {
        let buffer = buffer_at(&diamond(Vec::new()), 1, 80, 24);
        let connector = |y: u16, glyph: &str| {
            (0..80)
                .find(|&x| buffer[(x, y)].symbol() == glyph)
                .unwrap_or_else(|| panic!("no {glyph:?} on row {y}: {:?}", row(&buffer, y)))
        };
        // Rows: title, blank, codegen, connector, build and lint, connector, test.
        let from_build = connector(5, "╰");
        let from_lint = connector(5, "╯");
        assert_eq!(buffer[(from_build, 5)].fg, ACCENT, "build to test");
        assert_eq!(buffer[(from_build + 1, 5)].fg, ACCENT, "build to test");
        assert_eq!(buffer[(from_lint, 5)].fg, LINE, "lint to test");
        for x in 0..80 {
            if buffer[(x, 3)].symbol() != " " {
                assert_eq!(buffer[(x, 3)].fg, LINE, "codegen's edges, x={x}");
            }
        }
        let start = (0..80)
            .find(|&x| buffer[(x, 4)].symbol() == "b")
            .expect("the build node");
        assert_eq!(buffer[(start, 4)].bg, ACCENT, "the focused node");
        assert_eq!(buffer[(start, 4)].fg, Color::Rgb(0x24, 0x22, 0x1E));
        assert!(
            row(&buffer, 4).contains("[ ○ build ]"),
            "{:?}",
            row(&buffer, 4)
        );
    }

    /// The title names the target and counts the beams and layers.
    #[test]
    fn the_title_names_the_target_and_counts_beams_and_layers() {
        let buffer = buffer_at(&diamond(Vec::new()), 1, 60, 24);
        let title = row(&buffer, 0);
        assert!(title.starts_with(" GRAPH / test"), "{title:?}");
        assert!(
            title.trim_end().ends_with("4 beams · 3 layers"),
            "{title:?}"
        );
        let single = buffer_at(&run(&["solo"], &[], &["solo"], Vec::new()), 0, 60, 24);
        assert!(row(&single, 0).trim_end().ends_with("1 beam · 1 layer"));
    }

    /// Below the DAG: the focused beam's name, the beams that need it,
    /// its location, its description and its commands as written.
    #[test]
    fn the_commands_section_lists_the_focused_beam() {
        let state = diamond(vec![
            details("build", &["cargo build {{ profile }}", "cargo doc"]),
            details("test", &["cargo test"]),
        ]);
        let buffer = buffer_at(&state, 1, 80, 24);
        let all = rows(&buffer);
        let header = all
            .iter()
            .position(|line| line.contains("COMMANDS / build"))
            .unwrap_or_else(|| panic!("no COMMANDS header: {all:#?}"));
        assert!(all[header].contains("NEEDED BY test"), "{:?}", all[header]);
        assert!(
            all[header].trim_end().ends_with("Beamfile:12"),
            "{:?}",
            all[header]
        );
        assert!(all[header + 1].contains("Compile the workspace"));
        assert!(all[header + 2].contains("$ cargo build {{ profile }}"));
        assert!(all[header + 3].contains("$ cargo doc"));
        let dollar = all[header + 2].find('$').unwrap() as u16;
        let dollar_cell = &buffer[(dollar, header as u16 + 2)];
        assert_eq!(dollar_cell.fg, ACCENT);
        assert!(dollar_cell.modifier.contains(Modifier::BOLD));
        assert!(
            buffer[(dollar + 2, header as u16 + 2)]
                .modifier
                .contains(Modifier::BOLD)
        );

        // The target is needed by nothing: no label.
        let target = rows(&buffer_at(&state, 3, 80, 24));
        let header = target
            .iter()
            .find(|line| line.contains("COMMANDS / test"))
            .expect("the target's header");
        assert!(!header.contains("NEEDED BY"), "{header:?}");
    }

    /// A four-layer chain takes seven rows, nine with the title and the
    /// blank under it; `COMMANDS` needs five more. At 12 rows the DAG
    /// still fits whole and `COMMANDS` goes; at 6 rows the DAG scrolls
    /// to keep the focused node, at the bottom, on screen.
    #[test]
    fn commands_drop_before_the_graph_scrolls() {
        let names = ["beam0", "beam1", "beam2", "beam3"];
        let edges = [("beam1", "beam0"), ("beam2", "beam1"), ("beam3", "beam2")];
        let state = run(
            &names,
            &edges,
            &["beam3"],
            vec![details("beam3", &["make"])],
        );

        let roomy = rows(&buffer_at(&state, 3, 60, 14)).join("\n");
        assert!(roomy.contains("COMMANDS / beam3"), "{roomy}");

        let short = rows(&buffer_at(&state, 3, 60, 12)).join("\n");
        assert!(!short.contains("COMMANDS"), "{short}");
        assert!(
            short.contains("beam0") && short.contains("beam3"),
            "{short}"
        );

        // Below the title row, which names the target `beam3` itself.
        let dag = |focused: usize| rows(&buffer_at(&state, focused, 60, 6))[1..].join("\n");
        let scrolled = dag(3);
        assert!(
            scrolled.contains("beam3"),
            "the focused node stays: {scrolled}"
        );
        assert!(
            !scrolled.contains("beam0"),
            "the top scrolls away: {scrolled}"
        );
        let top = dag(0);
        assert!(top.contains("beam0") && !top.contains("beam3"), "{top}");
    }

    /// In a narrow pane nothing is written past its right edge: the
    /// counter goes first, the names are cut with `…`.
    #[test]
    fn a_narrow_pane_keeps_everything_inside_it() {
        let state = diamond(vec![details(
            "build",
            &["cargo build --workspace --all-targets"],
        )]);
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        let pane = Rect::new(10, 0, 16, 24);
        terminal
            .draw(|frame| draw(frame, pane, &state, &GraphState::new(1)))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        for y in 0..24 {
            for x in (0..10).chain(26..40) {
                assert_eq!(
                    buffer[(x, y)].symbol(),
                    " ",
                    "({x}, {y}): {:?}",
                    row(&buffer, y)
                );
            }
        }
        assert!(!row(&buffer, 0).contains("beams"), "{:?}", row(&buffer, 0));
        assert!(rows(&buffer).iter().any(|line| line.contains('…')));
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
    /// cannot tell a focused node from any other, nor its status colour:
    /// every node is bold in its status colour, the focused one is the
    /// selection bar (`on-accent` bold on `accent`, reversed bold in
    /// monochrome).
    #[test]
    fn nodes_take_their_status_colour_and_the_focused_one_is_the_bar() {
        use super::super::theme::{Token, fill};
        use alba_engine::BeamStatus;
        use std::time::Duration;

        let failed = BeamState::Done {
            status: BeamStatus::Failed { exit_code: 1 },
            duration: Duration::from_secs(1),
        };
        let mode = ColourMode::Indexed;
        assert_eq!(
            node_style(&failed, 2, 2, mode),
            fill(Token::Accent, mode)
                .patch(style(Token::OnAccent, mode))
                .bold()
        );
        assert_eq!(
            node_style(&failed, 1, 2, mode),
            style(Token::Error, mode).bold()
        );
        assert_eq!(
            node_style(&BeamState::Pending, 1, 2, mode),
            style(Token::TextMuted, mode).bold()
        );
        assert_eq!(
            node_style(&failed, 1, 2, ColourMode::Monochrome),
            Style::default().bold()
        );
        assert_eq!(
            node_style(&failed, 2, 2, ColourMode::Monochrome),
            Style::default().reversed().bold()
        );
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
                draw_segment(frame, area, top_x, bottom_x, Style::default());
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
