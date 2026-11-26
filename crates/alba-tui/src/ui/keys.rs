//! The key hints row: `key label` pairs, keys in `accent` bold, labels
//! in `text`, three spaces between pairs.
//!
//! In normal mode the row is built from a base chosen by the session
//! state, then `G follow` is inserted when the output is scrolled away
//! from its end and `s all streams` prepended when the output is
//! filtered. When the row is too narrow, pairs drop from the end, except
//! `? help` and `q quit`, which are always kept.
//!
//! Search, copy, graph and help replace the row entirely: search with
//! its prompt (`search_prompt`), the others with their key hints from
//! `help.rs`.

use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::logs::{Scroll, StreamFilter};
use crate::search::SearchState;
use crate::state::{AppState, Mode, Phase};

use super::beams::fit_name;
use super::help;
use super::theme::{ColourMode, Token, fill, style};

const PAIR_GAP: &str = "   ";
/// The pairs that never drop.
const KEPT: [&str; 2] = ["?", "q"];

/// The key hints for a row `width` cells wide.
pub fn line(state: &AppState, width: u16) -> Line<'static> {
    let width = width as usize;
    let mut pairs = match &state.mode {
        Mode::Normal => normal_pairs(state),
        Mode::Search(search) => return search_prompt(search, width, state.colour),
        Mode::Copy(_) => pairs_of(help::COPY_KEYS),
        Mode::Graph(_) => pairs_of(help::GRAPH_KEYS),
        Mode::Help => pairs_of(help::HELP_KEYS),
    };
    while row_width(&pairs) > width {
        let Some(last) = pairs
            .iter()
            .rposition(|(key, _)| !KEPT.contains(&key.as_str()))
        else {
            break;
        };
        pairs.remove(last);
    }
    render(&pairs, state.colour)
}

/// The search prompt: `/` in `accent` bold, the query bold, a block
/// cursor, then `i/n` muted, `Enter commit` and `Esc cancel`. The query
/// is what the reader is typing, so it is never dropped: the pairs after
/// it drop from the end first, then the query is cut by display width.
fn search_prompt(search: &SearchState, width: usize, mode: ColourMode) -> Line<'static> {
    let total = search.matches.len();
    let current = if total == 0 { 0 } else { search.current + 1 };
    let counter = format!("{current}/{total}");
    let mut pairs = vec![pair("Enter", "commit"), pair("Esc", "cancel")];
    // `/`, the query, the cursor cell.
    let prompt = 1 + search.query.width() + 1;
    // The cells taken after the cursor by the counter and `pairs`.
    let after = |pairs: &[(String, String)], with_counter: bool| {
        let counter = if with_counter {
            PAIR_GAP.len() + counter.width()
        } else {
            0
        };
        let pairs = if pairs.is_empty() {
            0
        } else {
            PAIR_GAP.len() + row_width(pairs)
        };
        counter + pairs
    };
    while !pairs.is_empty() && prompt + after(&pairs, true) > width {
        pairs.pop();
    }
    let with_counter = prompt + after(&pairs, true) <= width;
    let query = fit_name(
        &search.query,
        width.saturating_sub(2 + after(&pairs, with_counter)),
    );

    let cursor = match mode {
        ColourMode::Monochrome => Style::new().reversed(),
        _ => fill(Token::Text, mode),
    };
    let mut spans = vec![
        Span::styled("/", style(Token::Accent, mode).bold()),
        Span::styled(query, style(Token::Text, mode).bold()),
        Span::styled(" ", cursor),
    ];
    if with_counter {
        spans.push(Span::raw(PAIR_GAP));
        spans.push(Span::styled(counter, style(Token::TextMuted, mode)));
    }
    if !pairs.is_empty() {
        spans.push(Span::raw(PAIR_GAP));
        spans.extend(render(&pairs, mode).spans);
    }
    Line::from(spans)
}

/// A copy result taking the row over for its visible window: the
/// message in `text`, then `q quit`.
pub fn copy_result(message: &str, mode: ColourMode) -> Line<'static> {
    let mut spans = vec![
        Span::styled(message.to_string(), style(Token::Text, mode)),
        Span::raw(PAIR_GAP),
    ];
    spans.extend(render(&[pair("q", "quit")], mode).spans);
    Line::from(spans)
}

fn pair(key: &str, label: &str) -> (String, String) {
    (key.to_string(), label.to_string())
}

fn normal_pairs(state: &AppState) -> Vec<(String, String)> {
    // `w` toggles watching; in a watch session that is stopping it.
    let watch = if state.watch_enabled {
        "stop watching"
    } else {
        "watch"
    };
    let head = [
        pair("↑↓", "beam"),
        pair("/", "search"),
        pair("g", "graph"),
        pair("v", "copy"),
    ];
    let middle: Vec<(String, String)> = match &state.phase {
        Phase::Running { .. } => vec![pair("r", "rerun"), pair("c", "cancel"), pair("w", watch)],
        Phase::Finished => vec![pair("r", "rerun"), pair("f", "no cache"), pair("w", watch)],
        Phase::Waiting { .. } => vec![pair("r", "rerun"), pair("w", watch)],
        Phase::Parked => vec![pair("w", watch)],
    };
    let mut pairs: Vec<(String, String)> = head.into_iter().chain(middle).collect();
    pairs.extend([pair("?", "help"), pair("q", "quit")]);

    let paused = state
        .logs
        .get(&state.displayed_log_key())
        .is_some_and(|buffer| !matches!(buffer.scroll(), Scroll::Following));
    if paused {
        pairs.insert(1, pair("G", "follow"));
    }
    if state.effective_filter() != StreamFilter::All {
        pairs.insert(0, pair("s", "all streams"));
    }
    pairs
}

/// Pairs from a ` · `-separated text whose first word of every item is
/// the key (`Esc cancel`); an item with no space is a key alone.
fn pairs_of(text: &str) -> Vec<(String, String)> {
    text.split(" · ")
        .map(|item| match item.split_once(' ') {
            Some((key, label)) => pair(key, label),
            None => pair(item, ""),
        })
        .collect()
}

fn pair_width((key, label): &(String, String)) -> usize {
    key.width()
        + if label.is_empty() {
            0
        } else {
            1 + label.width()
        }
}

fn row_width(pairs: &[(String, String)]) -> usize {
    pairs.iter().map(pair_width).sum::<usize>() + PAIR_GAP.len() * pairs.len().saturating_sub(1)
}

fn render(pairs: &[(String, String)], mode: ColourMode) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, label)) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(PAIR_GAP));
        }
        spans.push(Span::styled(key.clone(), style(Token::Accent, mode).bold()));
        if !label.is_empty() {
            spans.push(Span::styled(format!(" {label}"), style(Token::Text, mode)));
        }
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;
    use alba_engine::{RunEvent, RunSummary};
    use alba_executors::{OutputLine, Stream};
    use std::time::Instant;

    fn id(name: &str) -> BeamId {
        BeamId(name.to_string())
    }

    fn started(watch: bool) -> AppState {
        let mut state = AppState::new("build", watch);
        state.apply(
            &RunEvent::RunStarted {
                targets: vec![id("build")],
                affected_by: None,
                beams: vec![id("build")],
                edges: Vec::new(),
                details: Vec::new(),
            },
            Instant::now(),
        );
        state
    }

    fn finished(watch: bool) -> AppState {
        let mut state = started(watch);
        state.apply(
            &RunEvent::RunFinished {
                summary: RunSummary::default(),
            },
            Instant::now(),
        );
        state
    }

    fn row(state: &AppState) -> String {
        line(state, 200).to_string()
    }

    #[test]
    fn key_hints_follow_the_session_state() {
        assert_eq!(
            row(&started(false)),
            "↑↓ beam   / search   g graph   v copy   r rerun   c cancel   w watch   ? help   q quit"
        );
        assert_eq!(
            row(&finished(false)),
            "↑↓ beam   / search   g graph   v copy   r rerun   f no cache   w watch   ? help   q quit"
        );
        let mut waiting = finished(true);
        waiting.apply(&RunEvent::WatchWaiting { files: 3 }, Instant::now());
        assert_eq!(
            row(&waiting),
            "↑↓ beam   / search   g graph   v copy   r rerun   w stop watching   ? help   q quit"
        );
        let mut parked = AppState::new("build", true);
        parked.apply(
            &RunEvent::ProjectBroken {
                diagnostic: "error: nope\n".to_string(),
                report: None,
            },
            Instant::now(),
        );
        assert_eq!(
            row(&parked),
            "↑↓ beam   / search   g graph   v copy   w stop watching   ? help   q quit"
        );
    }

    #[test]
    fn a_paused_output_offers_follow_and_a_filter_offers_all_streams() {
        let mut state = started(false);
        for index in 0..30 {
            state.apply(
                &RunEvent::BeamOutput {
                    id: id("build"),
                    line: OutputLine {
                        stream: Stream::Stdout,
                        text: format!("line {index}"),
                    },
                    replayed: false,
                },
                Instant::now(),
            );
        }
        state.logs.get_mut("build").unwrap().scroll_up(5);
        state.cycle_stream_filter();
        assert!(row(&state).starts_with("s all streams   ↑↓ beam   G follow   / search"));
    }

    #[test]
    fn key_hints_keep_help_and_quit_when_narrow() {
        let state = started(false);
        assert_eq!(
            line(&state, 40).to_string(),
            "↑↓ beam   / search   ? help   q quit"
        );
        assert_eq!(line(&state, 10).to_string(), "? help   q quit");
    }

    fn searching(query: &str) -> AppState {
        let mut state = started(false);
        state.enter_search();
        if let Mode::Search(search) = &mut state.mode {
            search.query = query.to_string();
        }
        state
    }

    /// The prompt: `/` accent bold, the query bold, a block cursor,
    /// then `i/n` muted, `Enter commit`, `Esc cancel`. A query with a
    /// space is still one item.
    #[test]
    fn the_search_prompt_reads_query_cursor_and_count() {
        let mode = ColourMode::TrueColour;
        let mut state = searching("unused import").with_colour(mode);
        if let Mode::Search(search) = &mut state.mode {
            search.matches = vec![3, 8];
            search.current = 1;
        }
        let hints = line(&state, 200);
        assert_eq!(
            hints.to_string(),
            "/unused import    2/2   Enter commit   Esc cancel"
        );
        let span = |content: &str| {
            hints
                .spans
                .iter()
                .find(|span| span.content == content)
                .unwrap_or_else(|| panic!("no span {content:?} in {hints:?}"))
                .style
        };
        assert_eq!(span("/"), style(Token::Accent, mode).bold());
        assert_eq!(span("unused import"), style(Token::Text, mode).bold());
        assert_eq!(hints.spans[2].content, " ");
        assert_eq!(hints.spans[2].style, fill(Token::Text, mode));
        assert_eq!(span("2/2"), style(Token::TextMuted, mode));
        assert_eq!(span("Enter"), style(Token::Accent, mode).bold());

        let mono = searching("x").with_colour(ColourMode::Monochrome);
        let hints = line(&mono, 200);
        assert_eq!(hints.spans[2].content, " ");
        assert_eq!(hints.spans[2].style, Style::new().reversed());
    }

    /// The query is what the reader is typing: the other pairs give way
    /// first, then the query is cut by display width, never dropped;
    /// `/` and the cursor stay.
    #[test]
    fn a_long_search_query_is_cut_not_dropped() {
        let query = "国".repeat(30);
        let state = searching(&query);
        assert_eq!(
            line(&state, 96).to_string(),
            format!("/{query}    0/0   Enter commit   Esc cancel")
        );
        assert_eq!(
            line(&state, 95).to_string(),
            format!("/{query}    0/0   Enter commit")
        );
        assert_eq!(line(&state, 82).to_string(), format!("/{query}    0/0"));
        assert_eq!(line(&state, 67).to_string(), format!("/{query} "));
        let cut = line(&state, 20).to_string();
        assert_eq!(cut, format!("/{}… ", "国".repeat(8)));
        assert!(cut.width() <= 20);
    }

    #[test]
    fn the_help_row_closes_help() {
        let mut state = started(false);
        state.mode = Mode::Help;
        assert_eq!(row(&state), "Esc close help");
    }

    /// Copy's, Graph's and Help's rows are `help.rs`'s shared constants.
    #[test]
    fn copy_graph_and_help_rows_come_from_the_shared_constants() {
        use crate::copy::CopyState;
        use crate::graph::GraphState;

        let mut state = started(false);
        for (mode, keys) in [
            (Mode::Copy(CopyState::new_at(0)), help::COPY_KEYS),
            (Mode::Graph(GraphState::new(0)), help::GRAPH_KEYS),
            (Mode::Help, help::HELP_KEYS),
        ] {
            state.mode = mode;
            assert_eq!(row(&state), keys.replace(" · ", PAIR_GAP));
        }
    }

    #[test]
    fn the_graph_row_offers_to_open_the_output() {
        use crate::graph::GraphState;

        let mut state = started(false);
        state.mode = Mode::Graph(GraphState::new(0));
        assert_eq!(row(&state), "↑↓←→ move   Enter open output   Esc back");
    }

    #[test]
    fn keys_are_accent_bold_and_labels_text() {
        let state = started(false).with_colour(ColourMode::Indexed);
        let hints = line(&state, 200);
        assert_eq!(hints.spans[0].content, "↑↓");
        assert_eq!(
            hints.spans[0].style,
            style(Token::Accent, ColourMode::Indexed).bold()
        );
        assert_eq!(
            hints.spans[1].style,
            style(Token::Text, ColourMode::Indexed)
        );
    }
}
