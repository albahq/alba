//! The graph view, in the right pane beside the beams: a title row,
//! the run's dependency DAG laid out by `graph::layers` as one row of
//! `[ glyph name ]` nodes per layer (dependencies above the beams that
//! need them), a three-row connector band between consecutive layers
//! (`│` under each parent, one joining row such as `└──┼──┘`, `│` into
//! each child), then `COMMANDS` for the focused beam. The edges on the
//! focused beam's path to the targets are drawn in `accent` (bold in
//! monochrome), the others in `line`. When the
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
use super::title;

/// The title row and the blank row under it; where the output pane
/// would have its definition row, one more, so the DAG starts level
/// with the output's first line. That alignment row is the first thing
/// a short pane gives back: it goes before `COMMANDS` does.
fn rows_above_dag(pane_height: u16, dag_rows: usize) -> u16 {
    let aligned = 3;
    let room = pane_height.saturating_sub(aligned) as usize;
    if super::output::has_definition_row(pane_height) && dag_rows + COMMANDS_MIN_ROWS <= room {
        aligned
    } else {
        2
    }
}

/// The blank rows between the DAG and `COMMANDS`.
const ROWS_ABOVE_COMMANDS: usize = 2;

/// The rows `COMMANDS` needs under the DAG, five in all: its blank
/// rows, its header and at least two lines. With fewer it is dropped
/// before the DAG ever scrolls.
const COMMANDS_MIN_ROWS: usize = ROWS_ABOVE_COMMANDS + 3;

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

    let dag_rows = node_row(layers.len() - 1) + 1;
    let above = rows_above_dag(area.height, dag_rows);
    let room = area.height.saturating_sub(above) as usize;
    let focused_row = layer_of(&layers, graph_state.focused).map_or(0, node_row);
    let visible = if dag_rows <= room {
        0..dag_rows
    } else {
        beams::window(focused_row, dag_rows, room)
    };
    let dag = Rect::new(
        area.x,
        area.y + above.min(area.height),
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
        let band = Band::crossing(&layers, boundary, state, &path, dag);
        for (part, glyphs) in band.rows().into_iter().enumerate() {
            if let Some(row_area) = row_rect(dag, &visible, connector_row(boundary, part)) {
                draw_band_row(frame, row_area, &glyphs, state.colour);
            }
        }
    }

    if dag_rows + COMMANDS_MIN_ROWS <= room {
        let below = (dag_rows + ROWS_ABOVE_COMMANDS) as u16;
        let section = Rect::new(dag.x, dag.y + below, dag.width, dag.height - below);
        draw_commands(frame, section, state, graph_state.focused);
    }
}

/// The connector band between two layers: a row of `│` under the
/// parents, the joining row, a row of `│` into the children.
const BAND_ROWS: usize = 3;

/// The DAG's own row of layer `layer_index`'s nodes: one node row per
/// layer, a connector band between consecutive layers.
fn node_row(layer_index: usize) -> usize {
    layer_index * (BAND_ROWS + 1)
}

/// The DAG's own row of part `part` (0 to 2) of the connector band
/// between layer `boundary` and the next one.
fn connector_row(boundary: usize, part: usize) -> usize {
    node_row(boundary) + 1 + part
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
    let target = Some(state.target.as_str()).filter(|target| !target.is_empty());
    if title::full_width(head, target) + 1 + counter.width() <= width {
        let room = width - counter.width() - 1;
        let spans = title::breadcrumb(head, target, Vec::new(), room, mode);
        return title::with_right(
            spans,
            Span::styled(counter, style(Token::TextMuted, mode)),
            width,
        );
    }
    Line::from(title::breadcrumb(head, target, Vec::new(), width, mode))
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
/// does not all fit, the location goes first, then the dependents are
/// cut and dropped, and the beam's name is cut last.
fn commands_header(state: &AppState, focused: usize, width: usize) -> Line<'static> {
    let mode = state.colour;
    let head = "COMMANDS";
    let beam = state.beams[focused].id.as_str();
    let location = state
        .details
        .get(beam)
        .and_then(|details| details.location.as_deref())
        .unwrap_or_default();
    let needed_by = dependents(state, focused);
    let needed_width = if needed_by.is_empty() {
        0
    } else {
        NEEDED_BY.width() + needed_by.width()
    };
    let whole = title::full_width(head, Some(beam)) + needed_width;
    let location = if !location.is_empty() && whole + 1 + location.width() <= width {
        location
    } else {
        ""
    };
    let room = if location.is_empty() {
        width
    } else {
        width - location.width() - 1
    };

    let mut spans = title::breadcrumb(head, Some(beam), Vec::new(), room, mode);
    let used: usize = spans.iter().map(Span::width).sum();
    if !needed_by.is_empty() && used + NEEDED_BY.width() < room {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            "NEEDED BY",
            style(Token::TextMuted, mode).bold(),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            fit_name(&needed_by, room - used - NEEDED_BY.width()),
            style(Token::Text, mode),
        ));
    }
    title::with_right(
        spans,
        Span::styled(location.to_string(), style(Token::TextMuted, mode)),
        width,
    )
}

/// What goes between the beam's name and its dependents.
const NEEDED_BY: &str = "    NEEDED BY ";

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

/// Which ways the lines leave one cell of the joining row, and whether
/// any of them is on the focused path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Ways {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    path: bool,
}

impl Ways {
    /// The square box-drawing glyph for these ways, `None` for an empty
    /// cell.
    fn glyph(self) -> Option<char> {
        let glyph = match (self.up, self.down, self.left, self.right) {
            (false, false, false, false) => return None,
            (true, true, false, false)
            | (true, false, false, false)
            | (false, true, false, false) => '│',
            (false, false, _, _) => '─',
            (true, false, false, true) => '└',
            (true, false, true, false) => '┘',
            (false, true, false, true) => '┌',
            (false, true, true, false) => '┐',
            (true, false, true, true) => '┴',
            (false, true, true, true) => '┬',
            (true, true, false, true) => '├',
            (true, true, true, false) => '┤',
            (true, true, true, true) => '┼',
        };
        Some(glyph)
    }
}

/// One connector band, column by column over the DAG's width: the
/// parents' drops, the joining row, the children's drops. Every edge
/// whose dependency's layer is at or above `boundary` and whose beam's
/// layer is below it crosses the band. An edge spanning several layers
/// drops straight through every intermediate band at its dependency's
/// own column, and only bends toward its beam's column in the band
/// immediately above that beam.
struct Band {
    above: Vec<Ways>,
    join: Vec<Ways>,
    below: Vec<Ways>,
}

impl Band {
    fn crossing(
        layers: &[Vec<usize>],
        boundary: usize,
        state: &AppState,
        path: &HashSet<(usize, usize)>,
        dag: Rect,
    ) -> Self {
        let width = dag.width as usize;
        let mut band = Band {
            above: vec![Ways::default(); width],
            join: vec![Ways::default(); width],
            below: vec![Ways::default(); width],
        };
        for edge @ &(beam, dependency) in &state.edges {
            let Some(dependency_layer) = layer_of(layers, dependency) else {
                continue;
            };
            let Some(beam_layer) = layer_of(layers, beam) else {
                continue;
            };
            if dependency_layer > boundary || beam_layer <= boundary {
                continue; // this edge does not cross this band
            }
            let top = (node_x(dag, layers, dependency_layer, dependency) - dag.x) as usize;
            let bottom = if beam_layer == boundary + 1 {
                (node_x(dag, layers, beam_layer, beam) - dag.x) as usize
            } else {
                top // still passing through: no bend until the beam's own band
            };
            band.add(top, bottom, path.contains(edge));
        }
        band
    }

    /// One edge from column `top` down to column `bottom`.
    fn add(&mut self, top: usize, bottom: usize, path: bool) {
        let mark = |ways: &mut Ways, apply: fn(&mut Ways)| {
            apply(ways);
            ways.path |= path;
        };
        mark(&mut self.above[top], |ways| ways.down = true);
        mark(&mut self.below[bottom], |ways| ways.up = true);
        mark(&mut self.join[top], |ways| ways.up = true);
        mark(&mut self.join[bottom], |ways| ways.down = true);
        let (left, right) = (top.min(bottom), top.max(bottom));
        if left < right {
            mark(&mut self.join[left], |ways| ways.right = true);
            mark(&mut self.join[right], |ways| ways.left = true);
            for ways in &mut self.join[left + 1..right] {
                mark(ways, |ways| {
                    ways.left = true;
                    ways.right = true;
                });
            }
        }
    }

    fn rows(self) -> [Vec<Ways>; BAND_ROWS] {
        [self.above, self.join, self.below]
    }
}

/// One row of a band: each cell's glyph in `line`, or in `accent` (bold
/// in monochrome, which has no accent) when any of its lines is on the
/// focused path.
fn draw_band_row(frame: &mut Frame, area: Rect, cells: &[Ways], mode: ColourMode) {
    let line = style(Token::Line, mode);
    let path = match mode {
        ColourMode::Monochrome => Style::new().bold(),
        _ => style(Token::Accent, mode),
    };
    for (offset, ways) in cells.iter().enumerate().take(area.width as usize) {
        let Some(glyph) = ways.glyph() else {
            continue;
        };
        let cell_style = if ways.path { path } else { line };
        frame.buffer_mut().set_string(
            area.x + offset as u16,
            area.y,
            glyph.to_string(),
            cell_style,
        );
    }
}

fn layer_of(layers: &[Vec<usize>], beam: usize) -> Option<usize> {
    layers.iter().position(|nodes| nodes.contains(&beam))
}

fn node_x(area: Rect, layers: &[Vec<usize>], layer_index: usize, beam: usize) -> u16 {
    let nodes = &layers[layer_index];
    // `beam` always belongs to `layers[layer_index]` here: its caller
    // (`Band::crossing`) looks it up via `layer_of` just before.
    let position = nodes.iter().position(|&node| node == beam).unwrap_or(0);
    let fraction = graph::slot_center_fraction(position, nodes.len());
    fraction_to_x(area, fraction)
}

/// Turns a `slot_center_fraction` into an actual column inside `area`,
/// clamped to `area`'s own last column: the one place both the node row
/// and the connector band convert "how far across" into "which column",
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

    /// Focused on `build`: its edge to `test` is the path, in `accent`,
    /// the join it shares with `lint` included; `lint`'s own edge and
    /// both edges from `codegen` stay `line`. The focused node itself is
    /// `on-accent` on `accent`.
    #[test]
    fn the_focused_path_is_accent() {
        let buffer = buffer_at(&diamond(Vec::new()), 1, 80, 24);
        let at = |y: u16, glyph: &str| {
            (0..80)
                .find(|&x| buffer[(x, y)].symbol() == glyph)
                .unwrap_or_else(|| panic!("no {glyph:?} on row {y}: {:?}", row(&buffer, y)))
        };
        // Rows: title, two blanks, codegen, three connector rows, build
        // and lint, three connector rows, test.
        let build = at(9, "└");
        let lint = at(9, "┘");
        let join = at(9, "┬");
        assert_eq!(buffer[(build, 8)].fg, ACCENT, "out of build");
        assert_eq!(buffer[(build, 9)].fg, ACCENT, "build's corner");
        assert_eq!(buffer[(build + 1, 9)].fg, ACCENT, "build to test");
        assert_eq!(buffer[(join, 9)].fg, ACCENT, "the shared join");
        assert_eq!(buffer[(join, 10)].fg, ACCENT, "into test");
        assert_eq!(buffer[(lint, 8)].fg, LINE, "out of lint");
        assert_eq!(buffer[(lint, 9)].fg, LINE, "lint's corner");
        assert_eq!(buffer[(lint - 1, 9)].fg, LINE, "lint to test");
        for y in 4..7 {
            for x in 0..80 {
                if buffer[(x, y)].symbol() != " " {
                    assert_eq!(buffer[(x, y)].fg, LINE, "codegen's edges, ({x}, {y})");
                }
            }
        }
        let start = (0..80)
            .find(|&x| buffer[(x, 7)].symbol() == "b")
            .expect("the build node");
        assert_eq!(buffer[(start, 7)].bg, ACCENT, "the focused node");
        assert_eq!(buffer[(start, 7)].fg, Color::Rgb(0x24, 0x22, 0x1E));
        assert!(
            row(&buffer, 7).contains("[ ○ build ]"),
            "{:?}",
            row(&buffer, 7)
        );
    }

    /// Monochrome has no accent: the path is bold instead.
    #[test]
    fn the_focused_path_is_bold_in_monochrome() {
        let state = diamond(Vec::new()).with_colour(ColourMode::Monochrome);
        let buffer = buffer_at(&state, 1, 80, 24);
        let at = |glyph: &str| (0..80).find(|&x| buffer[(x, 9)].symbol() == glyph).unwrap();
        assert!(buffer[(at("└"), 9)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(at("┘"), 9)].modifier.contains(Modifier::BOLD));
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

    /// A four-layer chain takes thirteen rows, fifteen with the title and
    /// the blank under it once the alignment row is given back;
    /// `COMMANDS` needs five more. At 16 rows the DAG
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

        let roomy = rows(&buffer_at(&state, 3, 60, 20)).join("\n");
        assert!(roomy.contains("COMMANDS / beam3"), "{roomy}");

        let tight = rows(&buffer_at(&state, 3, 60, 19)).join("\n");
        assert!(!tight.contains("COMMANDS"), "one row short: {tight}");

        let short = rows(&buffer_at(&state, 3, 60, 16)).join("\n");
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
        // The location goes first, then `NEEDED BY`; the name is cut last.
        let all = rows(&buffer).join("\n");
        assert!(all.contains("COMMANDS / bui…"), "{all}");
        assert!(
            !all.contains("Beamfile") && !all.contains("NEEDED"),
            "{all}"
        );
    }

    /// The text of row `y`, the cell after each wide character skipped.
    fn text(buffer: &Buffer, y: u16) -> String {
        let mut text = String::new();
        let mut x = 0;
        while x < buffer.area.width {
            let symbol = buffer[(x, y)].symbol();
            text.push_str(symbol);
            x += symbol.width().max(1) as u16;
        }
        text
    }

    /// A wide (CJK) target is cut by display width, in the title and in
    /// the `COMMANDS` header alike, and nothing passes the pane's edge.
    #[test]
    fn a_wide_name_is_cut_by_display_width() {
        let name = "国".repeat(10);
        let mut details = details(&name, &["make"]);
        details.location = None;
        let state = run(&[name.as_str()], &[], &[name.as_str()], vec![details]);
        // 20 cells: the pane's content is 19 from column 1.
        let buffer = buffer_at(&state, 0, 20, 24);
        let all: Vec<String> = (0..24).map(|y| text(&buffer, y)).collect();
        assert_eq!(all[0].trim(), "GRAPH / 国国国国国…", "{all:#?}");
        let header = all
            .iter()
            .find(|line| line.contains("COMMANDS"))
            .unwrap_or_else(|| panic!("{all:#?}"));
        assert_eq!(header.trim(), "COMMANDS / 国国国…");
        for line in &all {
            assert!(line.width() <= 20, "{line:?}");
        }
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

    /// The cells of row `y` from column `from` to `to`, inclusive.
    fn cells(buffer: &Buffer, y: u16, from: u16, to: u16) -> String {
        (from..=to).map(|x| buffer[(x, y)].symbol()).collect()
    }

    /// One parent, two children: a `│` under the parent, one joining
    /// row, a `│` into each child. In a 41-cell terminal the pane's
    /// content is 40 cells from column 1: `lib` centres on column 21,
    /// `app` and `cli` on columns 11 and 31.
    #[test]
    fn two_layers_join_through_three_connector_rows() {
        let state = run(
            &["lib", "app", "cli"],
            &[("app", "lib"), ("cli", "lib")],
            &["app", "cli"],
            Vec::new(),
        );
        let buffer = buffer_at(&state, 0, 41, 24);
        let bar = format!("{}│{}", " ".repeat(10), " ".repeat(10));
        assert_eq!(cells(&buffer, 4, 11, 31), bar, "under lib");
        assert_eq!(
            cells(&buffer, 5, 11, 31),
            format!("┌{}┴{}┐", "─".repeat(9), "─".repeat(9))
        );
        assert_eq!(
            cells(&buffer, 6, 11, 31),
            format!("│{}│", " ".repeat(19)),
            "into app and cli"
        );
    }

    /// Three parents, one child under the middle one: the join is the
    /// `└──┼──┘` row. 61 cells: the pane's content is 60 from column 1,
    /// the parents on columns 11, 31 and 51.
    #[test]
    fn parents_join_their_child_on_one_row() {
        let state = run(
            &["a", "b", "c", "d"],
            &[("d", "a"), ("d", "b"), ("d", "c")],
            &["d"],
            Vec::new(),
        );
        let buffer = buffer_at(&state, 0, 61, 24);
        assert_eq!(
            cells(&buffer, 5, 11, 51),
            format!("└{}┼{}┘", "─".repeat(19), "─".repeat(19))
        );
        assert_eq!(cells(&buffer, 6, 31, 31), "│");
    }

    /// `c` needs `a` (two layers up) and `b`; `b` needs `x`. The edge
    /// from `a` drops straight through the first band and past `b`'s
    /// row at `a`'s column, then bends to `c` on the second join, where
    /// it meets `b`'s own drop.
    #[test]
    fn an_edge_skipping_a_layer_still_renders() {
        let state = run(
            &["a", "b", "c", "x"],
            &[("c", "a"), ("c", "b"), ("b", "x")],
            &["c"],
            Vec::new(),
        );
        // 41 cells: `a` and `x` on columns 11 and 31, `b` and `c` on 21.
        // Focused on `a`: only its edge to `c` is the path.
        let buffer = buffer_at(&state, 0, 41, 24);
        for y in 4..7 {
            assert_eq!(cells(&buffer, y, 11, 11), "│", "a's drop, row {y}");
        }
        assert_eq!(
            cells(&buffer, 5, 21, 31),
            format!("┌{}┘", "─".repeat(9)),
            "x to b"
        );
        assert_eq!(cells(&buffer, 8, 11, 11), "│", "a passes b's band");
        assert_eq!(
            cells(&buffer, 9, 11, 21),
            format!("└{}┤", "─".repeat(9)),
            "a joins b's drop into c"
        );
        assert_eq!(cells(&buffer, 10, 21, 21), "│", "into c");

        for (x, y) in [
            (11, 4),
            (11, 5),
            (11, 6),
            (11, 8),
            (11, 9),
            (16, 9),
            (21, 9),
            (21, 10),
        ] {
            assert_eq!(buffer[(x, y)].fg, ACCENT, "a to c at ({x}, {y})");
        }
        for (x, y) in [(31, 4), (31, 5), (26, 5), (21, 5), (21, 6), (21, 8)] {
            assert_eq!(buffer[(x, y)].fg, LINE, "x to b and b to c at ({x}, {y})");
        }
    }

    /// Where the output pane would have its definition row, the DAG
    /// starts level with the output's first line, two rows under the
    /// title, and `COMMANDS` sits two blank rows under the DAG; a
    /// shorter pane gives one of the rows above the DAG back.
    #[test]
    fn the_dag_starts_with_the_output_and_commands_sits_two_rows_below() {
        let state = run(
            &["codegen", "build"],
            &[("build", "codegen")],
            &["build"],
            vec![details("build", &["make"])],
        );
        let line_of = |rows: &[String], needle: &str| {
            rows.iter()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("no {needle:?} in {rows:#?}"))
        };
        let roomy = rows(&buffer_at(&state, 1, 60, 20));
        assert_eq!(line_of(&roomy, "[ ○ codegen ]"), 3);
        assert_eq!(line_of(&roomy, "[ ○ build ]"), 7);
        assert_eq!(line_of(&roomy, "COMMANDS / build"), 10);
        let short = rows(&buffer_at(&state, 1, 60, 7));
        assert_eq!(line_of(&short, "[ ○ codegen ]"), 2);
    }

    /// The alignment row above the DAG goes before `COMMANDS` does: a
    /// three-layer chain in the 16-row pane of an 80x24 terminal keeps
    /// its `COMMANDS`, the DAG one row under the blank under the title.
    #[test]
    fn the_alignment_row_goes_before_commands() {
        let state = run(
            &["codegen", "build", "test"],
            &[("build", "codegen"), ("test", "build")],
            &["test"],
            vec![details("build", &["make"])],
        );
        let all = rows(&buffer_at(&state, 1, 80, 16));
        let line_of = |needle: &str| all.iter().position(|row| row.contains(needle));
        assert_eq!(line_of("[ ○ codegen ]"), Some(2), "{all:#?}");
        assert!(line_of("COMMANDS / build").is_some(), "{all:#?}");
    }
}
