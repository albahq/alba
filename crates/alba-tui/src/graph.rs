//! The run's dependency graph, laid out into layers for the graph view.
//!
//! Pure by design: no I/O, no terminal, no clock. `layers` turns
//! `AppState.edges` — `(beam, dependency)` index pairs into `AppState.beams`,
//! guaranteed by the engine's `RunStarted` snapshot to describe a
//! validated DAG with both ends of every edge inside the subgraph — into
//! rows a renderer can draw top to bottom. `GraphState` is the one bit of
//! state the graph view owns: which node currently has the focus.

use crossterm::event::KeyCode;

/// Topological layering of the run's snapshot: layer 0 is the beams with
/// no dependencies in the subgraph; a beam sits one layer above its
/// *deepest* dependency (`layer[i] = 1 + max(layer[dependency])`),
/// computed by iterating every edge to a fixpoint. The graph is already a
/// validated DAG by the time it reaches here (the engine's `RunStarted`
/// snapshot guarantees it), so no cycle guard is needed — the fixpoint is
/// reached in at most `beam_count` passes over `edges`.
///
/// Each returned layer lists its beams in ascending index order, which is
/// also the order `ui/graphpane.rs` lays them out left to right.
pub fn layers(beam_count: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    if beam_count == 0 {
        return Vec::new();
    }
    let mut layer = vec![0usize; beam_count];
    let mut changed = true;
    while changed {
        changed = false;
        for &(beam, dependency) in edges {
            let candidate = layer[dependency] + 1;
            if layer[beam] < candidate {
                layer[beam] = candidate;
                changed = true;
            }
        }
    }
    let layer_count = layer.iter().copied().max().unwrap_or(0) + 1;
    let mut result = vec![Vec::new(); layer_count];
    for (beam, &beam_layer) in layer.iter().enumerate() {
        result[beam_layer].push(beam);
    }
    result
}

/// The horizontal slot a node at `index` occupies among `count` nodes
/// laid out evenly across a row, as a fraction of the row's own width
/// (0.0 at the left edge, 1.0 at the right). `ui/graphpane.rs` turns this
/// into an actual column by multiplying it by the render area's width,
/// and `GraphState::navigate` compares it directly to judge "nearest by
/// horizontal position" — the one shared computation both sides go
/// through, the way `copy::scroll_window` mirrors `LogBuffer::view` so
/// hit-testing and rendering can never disagree about what is on screen.
pub fn slot_center_fraction(index: usize, count: usize) -> f64 {
    if count <= 1 {
        0.5
    } else {
        (index as f64 + 0.5) / count as f64
    }
}

/// The graph view's own state: which beam (an index into `AppState.beams`,
/// same as everywhere else) currently has the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphState {
    pub focused: usize,
}

impl GraphState {
    /// Opens the graph focused on whatever beam is already selected, so
    /// the reader lands on the node they were just looking at rather than
    /// always starting at layer 0.
    pub fn new(selected: usize) -> Self {
        Self { focused: selected }
    }

    /// Moves the focus: left/right step to the neighbouring node inside
    /// the same layer (a no-op at either end); up/down cross to the
    /// nearest node in the layer above/below, "nearest" meaning by
    /// horizontal position (`slot_center_fraction`), not by index — a
    /// naive same-index match would jump to an unrelated node whenever
    /// the two layers hold different numbers of beams. A no-op if
    /// `self.focused` is not actually in `layers` (nothing to navigate
    /// from), or at either end of the stack of layers.
    pub fn navigate(&mut self, key: KeyCode, layers: &[Vec<usize>]) {
        let Some((layer_index, position)) = locate(self.focused, layers) else {
            return;
        };
        let nodes = &layers[layer_index];
        match key {
            KeyCode::Left => {
                if position > 0 {
                    self.focused = nodes[position - 1];
                }
            }
            KeyCode::Right => {
                if position + 1 < nodes.len() {
                    self.focused = nodes[position + 1];
                }
            }
            KeyCode::Up => {
                if let Some(target_layer) = layer_index.checked_sub(1) {
                    self.cross_to(layers, nodes.len(), position, target_layer);
                }
            }
            KeyCode::Down => {
                let target_layer = layer_index + 1;
                if target_layer < layers.len() {
                    self.cross_to(layers, nodes.len(), position, target_layer);
                }
            }
            _ => {}
        }
    }

    fn cross_to(
        &mut self,
        layers: &[Vec<usize>],
        source_count: usize,
        source_position: usize,
        target_layer: usize,
    ) {
        let fraction = slot_center_fraction(source_position, source_count);
        let target_nodes = &layers[target_layer];
        let target_position = nearest_slot(fraction, target_nodes.len());
        self.focused = target_nodes[target_position];
    }
}

/// `(layer index, position within it)` of `focused` inside `layers`, or
/// `None` when it names no node there at all.
fn locate(focused: usize, layers: &[Vec<usize>]) -> Option<(usize, usize)> {
    layers.iter().enumerate().find_map(|(layer_index, nodes)| {
        nodes
            .iter()
            .position(|&beam| beam == focused)
            .map(|position| (layer_index, position))
    })
}

/// The index, among `count` evenly laid-out slots, whose centre
/// (`slot_center_fraction`) is closest to `fraction`. `count == 0` cannot
/// happen for a layer `layers` actually produced (every layer holds at
/// least one beam), but returns 0 rather than panicking if it ever did.
fn nearest_slot(fraction: f64, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    (0..count)
        .min_by(|&a, &b| {
            let distance_a = (slot_center_fraction(a, count) - fraction).abs();
            let distance_b = (slot_center_fraction(b, count) - fraction).abs();
            distance_a.total_cmp(&distance_b)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_follow_the_longest_path() {
        // build -> codegen, test -> build: codegen | build | test
        let layered = layers(3, &[(1, 0), (2, 1)]);
        assert_eq!(layered, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn independent_beams_share_a_layer() {
        let layered = layers(3, &[(2, 0), (2, 1)]);
        assert_eq!(layered, vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn an_edge_skipping_a_layer_still_places_by_the_deepest_path() {
        // d needs a and c; c needs b; b needs a: a | b | c | d
        let layered = layers(4, &[(3, 0), (3, 2), (2, 1), (1, 0)]);
        assert_eq!(layered, vec![vec![0], vec![1], vec![2], vec![3]]);
    }

    /// A run with no beams at all (before the first `RunStarted`) has no
    /// layer to draw, rather than one spurious empty layer.
    #[test]
    fn an_empty_beam_set_has_no_layers() {
        assert_eq!(layers(0, &[]), Vec::<Vec<usize>>::new());
    }

    /// A single beam with no edges is its own, whole layer — the
    /// smallest graph the view has to draw.
    #[test]
    fn a_single_beam_with_no_edges_is_its_own_layer() {
        assert_eq!(layers(1, &[]), vec![vec![0]]);
    }

    #[test]
    fn new_focuses_the_beam_passed_in() {
        let state = GraphState::new(2);
        assert_eq!(state.focused, 2);
    }

    /// Left/right step to the neighbouring node in the same layer, and
    /// stop rather than wrap at either end.
    #[test]
    fn left_and_right_move_within_a_layer_and_stop_at_the_ends() {
        // one beam (0) with three dependents (1, 2, 3): layers [[0],[1,2,3]]
        let layered = layers(4, &[(1, 0), (2, 0), (3, 0)]);
        let mut state = GraphState::new(1);

        state.navigate(KeyCode::Left, &layered);
        assert_eq!(state.focused, 1, "already at the left end");

        state.navigate(KeyCode::Right, &layered);
        assert_eq!(state.focused, 2);
        state.navigate(KeyCode::Right, &layered);
        assert_eq!(state.focused, 3);
        state.navigate(KeyCode::Right, &layered);
        assert_eq!(state.focused, 3, "already at the right end");

        state.navigate(KeyCode::Left, &layered);
        assert_eq!(state.focused, 2);
    }

    /// Up/down cross layers to the nearest node by horizontal position,
    /// not by index — the layer above has only one node, so every node
    /// below must cross to that same one node.
    #[test]
    fn down_and_up_cross_layers_by_nearest_horizontal_position() {
        // beam 0 alone in layer 0; beams 1, 2, 3 (in that order) share
        // layer 1 as its three dependents.
        let layered = layers(4, &[(1, 0), (2, 0), (3, 0)]);
        assert_eq!(layered, vec![vec![0], vec![1, 2, 3]]);

        let mut state = GraphState::new(0);
        state.navigate(KeyCode::Down, &layered);
        assert_eq!(
            state.focused, 2,
            "the single node above centres on the middle of the three below"
        );

        state.navigate(KeyCode::Up, &layered);
        assert_eq!(state.focused, 0, "the only node in the layer above");
    }

    /// Up from the top layer, or down from the bottom one, is a no-op:
    /// there is nowhere to cross to.
    #[test]
    fn navigating_past_the_top_or_bottom_layer_does_nothing() {
        let layered = layers(2, &[(1, 0)]);
        let mut top = GraphState::new(0);
        top.navigate(KeyCode::Up, &layered);
        assert_eq!(top.focused, 0);

        let mut bottom = GraphState::new(1);
        bottom.navigate(KeyCode::Down, &layered);
        assert_eq!(bottom.focused, 1);
    }

    /// A focus that names no node in `layers` at all (should never
    /// happen given a validated DAG, but must not panic) leaves
    /// navigation inert.
    #[test]
    fn navigating_with_an_unknown_focus_does_nothing() {
        let layered = layers(2, &[(1, 0)]);
        let mut state = GraphState::new(99);
        state.navigate(KeyCode::Right, &layered);
        assert_eq!(state.focused, 99);
    }

    #[test]
    fn slot_center_fraction_centres_a_single_node() {
        assert_eq!(slot_center_fraction(0, 1), 0.5);
    }

    #[test]
    fn slot_center_fraction_spreads_nodes_evenly() {
        assert_eq!(slot_center_fraction(0, 2), 0.25);
        assert_eq!(slot_center_fraction(1, 2), 0.75);
    }
}
