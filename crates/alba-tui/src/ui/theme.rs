//! The one place a status becomes a colour: the Alba palette as eleven
//! tokens, drawn in one of three colour modes chosen once at start.
//! Monochrome answers `Style::default()` for everything but muted text
//! (dim), so `NO_COLOR` renders the monochrome interface it always did.

use alba_engine::BeamStatus;
use ratatui::style::{Color, Style, Stylize};

use crate::state::BeamState;

/// How the interface colours itself, picked once at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColourMode {
    /// The palette exactly, as 24-bit colour.
    TrueColour,
    /// Each token mapped to its nearest xterm-256 index.
    Indexed,
    /// No colour at all: hierarchy falls back to bold, reversed and dim.
    #[default]
    Monochrome,
}

/// The palette's eleven colours, one token per role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Surface,
    Text,
    TextMuted,
    Track,
    Line,
    Accent,
    OnAccent,
    Success,
    Cached,
    Error,
    Selection,
}

impl Token {
    fn rgb(self) -> (u8, u8, u8) {
        match self {
            Token::Surface => (0x15, 0x17, 0x19),
            Token::Text => (0xE3, 0xE4, 0xDC),
            Token::TextMuted => (0xA1, 0xA7, 0xA4),
            Token::Track => (0x79, 0x80, 0x7C),
            Token::Line => (0x62, 0x6A, 0x67),
            Token::Accent => (0xF3, 0xC2, 0x80),
            Token::OnAccent => (0x24, 0x22, 0x1E),
            Token::Success => (0xA5, 0xCF, 0x9A),
            Token::Cached => (0x9C, 0xCB, 0xC9),
            Token::Error => (0xF0, 0xA2, 0x9A),
            Token::Selection => (0x35, 0x3C, 0x40),
        }
    }

    /// The token's xterm-256 index, from [`INDEXED`].
    fn index(self) -> u8 {
        INDEXED[self as usize]
    }
}

/// The nearest xterm-256 index of each token, in `Token` order. Computed
/// once by Euclidean RGB distance over the 6x6x6 cube (16-231) and the
/// grayscale ramp (232-255), never the 0-15 system colours, which every
/// terminal draws differently. `indexed_tokens_use_the_pinned_table`
/// recomputes it and fails if the two ever disagree.
///
/// One override: `Accent` and `Error` both land on 216 by distance, and
/// a running beam must not look like a failed one, so `Accent` takes
/// 222 (#FFD787), the next nearest choice that keeps its hue. Every other
/// token keeps its nearest index; `indexed_tokens_are_pairwise_distinct`
/// guards against any collision.
const INDEXED: [u8; 11] = [233, 254, 248, 244, 241, 222, 235, 150, 152, 216, 237];

fn colour(token: Token, mode: ColourMode) -> Option<Color> {
    match mode {
        ColourMode::TrueColour => {
            let (r, g, b) = token.rgb();
            Some(Color::Rgb(r, g, b))
        }
        ColourMode::Indexed => Some(Color::Indexed(token.index())),
        ColourMode::Monochrome => None,
    }
}

/// The token as a foreground. Monochrome has no colour, except that
/// muted text is dim.
pub fn style(token: Token, mode: ColourMode) -> Style {
    match colour(token, mode) {
        Some(colour) => Style::new().fg(colour),
        None if token == Token::TextMuted => Style::new().dim(),
        None => Style::default(),
    }
}

/// The token as a background; monochrome paints none.
pub fn fill(token: Token, mode: ColourMode) -> Style {
    match colour(token, mode) {
        Some(colour) => Style::new().bg(colour),
        None => Style::default(),
    }
}

/// The token a command's ANSI base colour is drawn as, `None` for the
/// colours that stay as the command wrote them (blue, magenta).
fn ansi_token(colour: Color) -> Option<Token> {
    match colour {
        Color::Red | Color::LightRed => Some(Token::Error),
        Color::Green | Color::LightGreen => Some(Token::Success),
        Color::Yellow | Color::LightYellow => Some(Token::Accent),
        Color::Cyan | Color::LightCyan => Some(Token::Cached),
        Color::White | Color::Gray | Color::Reset => Some(Token::Text),
        Color::Black | Color::DarkGray => Some(Token::TextMuted),
        _ => None,
    }
}

/// A command's own style drawn on the palette: the sixteen base
/// foreground colours become tokens (`Reset`, the default foreground,
/// becomes `Text`), indexed and RGB colours and every background stay as
/// written, and monochrome drops every colour, underline colour included
/// (modifiers stay).
pub fn remap_ansi(style: Style, mode: ColourMode) -> Style {
    if mode == ColourMode::Monochrome {
        return Style {
            fg: None,
            bg: None,
            underline_color: None,
            ..style
        };
    }
    let fg = match style.fg {
        Some(colour) => match ansi_token(colour) {
            Some(token) => self::colour(token, mode),
            None => Some(colour),
        },
        None => None,
    };
    Style { fg, ..style }
}

/// The token a beam's status is drawn in; pending and cancelled beams
/// are muted.
pub fn status_token(state: &BeamState) -> Token {
    match state {
        BeamState::Pending => Token::TextMuted,
        BeamState::Running { .. } => Token::Accent,
        BeamState::Done { status, .. } => match status {
            BeamStatus::Succeeded => Token::Success,
            BeamStatus::Cached => Token::Cached,
            BeamStatus::Failed { .. } | BeamStatus::FailedAllowed { .. } => Token::Error,
            BeamStatus::Cancelled => Token::TextMuted,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_engine::BeamStatus;
    use std::time::{Duration, Instant};

    const ALL: [(Token, (u8, u8, u8)); 11] = [
        (Token::Surface, (0x15, 0x17, 0x19)),
        (Token::Text, (0xE3, 0xE4, 0xDC)),
        (Token::TextMuted, (0xA1, 0xA7, 0xA4)),
        (Token::Track, (0x79, 0x80, 0x7C)),
        (Token::Line, (0x62, 0x6A, 0x67)),
        (Token::Accent, (0xF3, 0xC2, 0x80)),
        (Token::OnAccent, (0x24, 0x22, 0x1E)),
        (Token::Success, (0xA5, 0xCF, 0x9A)),
        (Token::Cached, (0x9C, 0xCB, 0xC9)),
        (Token::Error, (0xF0, 0xA2, 0x9A)),
        (Token::Selection, (0x35, 0x3C, 0x40)),
    ];

    fn done(status: BeamStatus) -> BeamState {
        BeamState::Done {
            status,
            duration: Duration::from_secs(1),
        }
    }

    #[test]
    fn truecolour_tokens_are_the_spec_values() {
        for (token, (r, g, b)) in ALL {
            assert_eq!(
                style(token, ColourMode::TrueColour).fg,
                Some(Color::Rgb(r, g, b)),
                "{token:?}"
            );
            assert_eq!(
                fill(token, ColourMode::TrueColour).bg,
                Some(Color::Rgb(r, g, b)),
                "{token:?}"
            );
        }
    }

    /// The nearest cube or grayscale index by squared RGB distance, the
    /// first of equals: the method the pinned table documents.
    fn nearest_index((r, g, b): (u8, u8, u8)) -> u8 {
        const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
        let cube = (0..216u16).map(|i| {
            let i = i as usize;
            (
                16 + i as u8,
                (LEVELS[i / 36], LEVELS[i / 6 % 6], LEVELS[i % 6]),
            )
        });
        let ramp = (0..24u8).map(|i| {
            let level = 8 + 10 * i as i32;
            (232 + i, (level, level, level))
        });
        cube.chain(ramp)
            .min_by_key(|(_, (cr, cg, cb))| {
                (cr - r as i32).pow(2) + (cg - g as i32).pow(2) + (cb - b as i32).pow(2)
            })
            .map(|(index, _)| index)
            .unwrap()
    }

    #[test]
    fn indexed_tokens_use_the_pinned_table() {
        let pinned = [
            (Token::Surface, 233),
            (Token::Text, 254),
            (Token::TextMuted, 248),
            (Token::Track, 244),
            (Token::Line, 241),
            (Token::Accent, 222),
            (Token::OnAccent, 235),
            (Token::Success, 150),
            (Token::Cached, 152),
            (Token::Error, 216),
            (Token::Selection, 237),
        ];
        for (token, index) in pinned {
            assert_eq!(
                style(token, ColourMode::Indexed).fg,
                Some(Color::Indexed(index)),
                "{token:?}"
            );
            assert_eq!(
                fill(token, ColourMode::Indexed).bg,
                Some(Color::Indexed(index)),
                "{token:?}"
            );
        }
        for (token, rgb) in ALL {
            // Accent is the one documented override of "nearest".
            let expected = if token == Token::Accent {
                222
            } else {
                nearest_index(rgb)
            };
            assert_eq!(token.index(), expected, "{token:?}");
        }
    }

    #[test]
    fn indexed_tokens_are_pairwise_distinct() {
        for (i, (first, _)) in ALL.iter().enumerate() {
            for (second, _) in &ALL[i + 1..] {
                assert_ne!(first.index(), second.index(), "{first:?} vs {second:?}");
            }
        }
    }

    #[test]
    fn monochrome_has_no_colour() {
        for (token, _) in ALL {
            assert_eq!(fill(token, ColourMode::Monochrome), Style::default());
            if token != Token::TextMuted {
                assert_eq!(style(token, ColourMode::Monochrome), Style::default());
            }
        }
        assert_eq!(
            style(Token::Error, ColourMode::Monochrome),
            Style::default()
        );
        assert_eq!(
            style(Token::TextMuted, ColourMode::Monochrome),
            Style::new().dim()
        );
    }

    #[test]
    fn ansi_base_colours_become_their_spec_tokens() {
        let rows = [
            (Color::Red, Token::Error),
            (Color::LightRed, Token::Error),
            (Color::Green, Token::Success),
            (Color::LightGreen, Token::Success),
            (Color::Yellow, Token::Accent),
            (Color::LightYellow, Token::Accent),
            (Color::Cyan, Token::Cached),
            (Color::LightCyan, Token::Cached),
            (Color::Gray, Token::Text),
            (Color::White, Token::Text),
            (Color::Reset, Token::Text),
            (Color::Black, Token::TextMuted),
            (Color::DarkGray, Token::TextMuted),
        ];
        for mode in [ColourMode::TrueColour, ColourMode::Indexed] {
            for (colour, token) in rows {
                assert_eq!(
                    remap_ansi(Style::new().fg(colour), mode),
                    style(token, mode),
                    "{colour:?} in {mode:?}"
                );
            }
            assert_eq!(
                remap_ansi(Style::new().fg(Color::Green).bold(), mode),
                style(Token::Success, mode).bold()
            );
        }
    }

    #[test]
    fn ansi_red_becomes_the_error_token() {
        for mode in [ColourMode::TrueColour, ColourMode::Indexed] {
            assert_eq!(
                remap_ansi(Style::new().fg(Color::Red), mode),
                style(Token::Error, mode)
            );
        }
    }

    /// The table is a foreground table: a command's own background is
    /// drawn as written.
    #[test]
    fn command_backgrounds_are_left_alone() {
        for mode in [ColourMode::TrueColour, ColourMode::Indexed] {
            for colour in [
                Color::Red,
                Color::Reset,
                Color::Indexed(9),
                Color::Rgb(1, 2, 3),
            ] {
                assert_eq!(
                    remap_ansi(Style::new().bg(colour), mode),
                    Style::new().bg(colour)
                );
            }
        }
    }

    #[test]
    fn ansi_blue_is_left_alone() {
        for mode in [ColourMode::TrueColour, ColourMode::Indexed] {
            for colour in [
                Color::Blue,
                Color::LightBlue,
                Color::Magenta,
                Color::LightMagenta,
                Color::Indexed(33),
                Color::Rgb(1, 2, 3),
            ] {
                assert_eq!(
                    remap_ansi(Style::new().fg(colour), mode),
                    Style::new().fg(colour)
                );
            }
        }
    }

    #[test]
    fn monochrome_drops_command_colours_and_keeps_modifiers() {
        assert_eq!(
            remap_ansi(
                Style::new()
                    .fg(Color::Rgb(1, 2, 3))
                    .bg(Color::Red)
                    .underline_color(Color::Green)
                    .bold(),
                ColourMode::Monochrome
            ),
            Style::new().bold()
        );
    }

    #[test]
    fn each_status_has_its_token() {
        assert_eq!(status_token(&done(BeamStatus::Succeeded)), Token::Success);
        assert_eq!(status_token(&done(BeamStatus::Cached)), Token::Cached);
        assert_eq!(
            status_token(&BeamState::Running {
                since: Instant::now()
            }),
            Token::Accent
        );
        assert_eq!(
            status_token(&done(BeamStatus::Failed { exit_code: 1 })),
            Token::Error
        );
        assert_eq!(
            status_token(&done(BeamStatus::FailedAllowed { exit_code: 1 })),
            Token::Error
        );
        assert_eq!(status_token(&BeamState::Pending), Token::TextMuted);
        assert_eq!(status_token(&done(BeamStatus::Cancelled)), Token::TextMuted);
    }
}
