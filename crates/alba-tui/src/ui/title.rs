//! The section-title pattern shared by the output pane, the graph and
//! its `COMMANDS` section: a bold head (`OUTPUT`, `GRAPH`, `COMMANDS`),
//! ` / ` in `line`, a name in `accent` bold cut to the room left, then
//! padding and a right-aligned text. What each title drops first when the
//! row is narrow stays with its caller, which decides the room it passes.

use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::beams::fit_name;
use super::theme::{ColourMode, Token, style};

const SEPARATOR: &str = " / ";

/// `head / name` within `room` cells, then `extra` when the whole of it
/// fits too. The head is left out when it does not fit; the name is cut
/// with `…` to what remains, and left out when nothing does.
pub(super) fn breadcrumb(
    head: &'static str,
    name: Option<&str>,
    extra: Vec<Span<'static>>,
    room: usize,
    mode: ColourMode,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if head.width() > room {
        return spans;
    }
    spans.push(Span::styled(head, style(Token::Text, mode).bold()));
    let Some(name) = name else {
        return spans;
    };
    let lead = full_width(head, Some(name)) - name.width();
    if room <= lead {
        return spans;
    }
    spans.push(Span::styled(SEPARATOR, style(Token::Line, mode)));
    spans.push(Span::styled(
        fit_name(name, room - lead),
        style(Token::Accent, mode).bold(),
    ));
    let extra_width: usize = extra.iter().map(Span::width).sum();
    if full_width(head, Some(name)) + extra_width <= room {
        spans.extend(extra);
    }
    spans
}

/// The cells `head / name` takes uncut.
pub(super) fn full_width(head: &str, name: Option<&str>) -> usize {
    head.width() + name.map_or(0, |name| SEPARATOR.width() + name.width())
}

/// `spans`, padded so `right` ends at `width`.
pub(super) fn with_right(
    mut spans: Vec<Span<'static>>,
    right: Span<'static>,
    width: usize,
) -> Line<'static> {
    let used: usize = spans.iter().map(Span::width).sum();
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used + right.width())),
    ));
    spans.push(right);
    Line::from(spans)
}
