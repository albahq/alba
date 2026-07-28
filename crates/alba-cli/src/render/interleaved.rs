//! The interleaved renderer: every line, from every beam, as it happens,
//! prefixed with the beam it came from (`build │ Compiling alba v0.1.0`).
//!
//! The default on a terminal, where seeing a long run make progress
//! matters more than reading each beam's output as one block.

use std::io;

use alba_engine::RunEvent;
use owo_colors::{AnsiColors, OwoColorize};

use super::{LineSink, Renderer, format_duration, print_summary, status_label};

/// The colours beam ids cycle through. Six is enough to tell adjacent
/// beams apart without reaching for colours that carry their own meaning
/// (plain red reads as "this failed", which a beam id must not imply).
const PALETTE: [AnsiColors; 6] = [
    AnsiColors::Cyan,
    AnsiColors::Green,
    AnsiColors::Yellow,
    AnsiColors::Blue,
    AnsiColors::Magenta,
    AnsiColors::BrightCyan,
];

/// Separates a beam id from the line it prefixes.
const SEPARATOR: &str = "\u{2502}";

pub struct InterleavedRenderer {
    color: bool,
    out: LineSink<io::Stdout>,
    err: LineSink<io::Stderr>,
}

impl InterleavedRenderer {
    /// `color` comes from [`crate::color_enabled`] — the single TTY and
    /// `NO_COLOR` gate the whole binary shares.
    pub fn new(color: bool) -> Self {
        Self {
            color,
            out: LineSink::stdout(),
            err: LineSink::stderr(),
        }
    }

    /// Prints one prefixed line on stdout.
    fn line(&mut self, id: &str, text: &str) {
        let prefix = if self.color {
            id.color(color_for(id)).to_string()
        } else {
            id.to_string()
        };
        self.out.line(&format!("{prefix} {SEPARATOR} {text}"));
    }
}

impl Renderer for InterleavedRenderer {
    fn handle(&mut self, event: &RunEvent) {
        match event {
            RunEvent::BeamStarted { id } => self.line(&id.0, "started"),
            RunEvent::BeamOutput { id, line } => self.line(&id.0, &line.text),
            RunEvent::BeamFinished {
                id,
                status,
                duration,
            } => {
                // `in` rather than a second parenthesis: a status is
                // already parenthesized (`failed (exit 7)`), and
                // `failed (exit 7) (1.2s)` reads as two unrelated asides.
                let text = format!("{} in {}", status_label(status), format_duration(*duration));
                self.line(&id.0, &text);
            }
            RunEvent::RunFinished { summary } => print_summary(&mut self.err, summary),
        }
    }
}

/// The colour a beam id is always drawn in.
///
/// Derived from the id's own bytes, so it is the same on every run of the
/// same project and does not depend on the order beams start, finish, or
/// happen to be iterated in — this project has already shipped two
/// non-determinism defects caused by exactly that kind of incidental
/// ordering. A collision between two ids is harmless (they merely share a
/// colour), which is why a plain hash is enough and no allocation-order
/// bookkeeping is needed.
fn color_for(id: &str) -> AnsiColors {
    PALETTE[(fnv1a(id) % PALETTE.len() as u64) as usize]
}

/// FNV-1a, written out rather than taken from `std`: `DefaultHasher`'s
/// output is explicitly not guaranteed stable across Rust releases, and a
/// beam's colour changing when the toolchain is upgraded is exactly the
/// instability this function exists to prevent.
fn fnv1a(text: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    text.bytes().fold(OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AnsiColors` is compared through its `Debug` form: the palette
    /// entry itself is only ever handed to `owo-colors`, and naming it in
    /// a test would pin these assertions to which colour was picked rather
    /// than to the property being asserted.
    fn color_name(id: &str) -> String {
        format!("{:?}", color_for(id))
    }

    /// The property that matters: an id's colour is a function of the id
    /// alone, so it cannot vary with the order beams are encountered in.
    #[test]
    fn a_beam_id_always_gets_the_same_color() {
        assert_eq!(color_name("build"), color_name("build"));
        assert_eq!(color_name("api:test"), color_name("api:test"));
    }

    #[test]
    fn different_ids_spread_across_the_palette() {
        let ids = ["build", "test", "lint", "deploy", "docs", "bench"];
        let used: std::collections::HashSet<_> = ids.iter().map(|id| color_name(id)).collect();
        assert!(
            used.len() > 1,
            "a palette that collapses every id to one colour is useless"
        );
    }
}
