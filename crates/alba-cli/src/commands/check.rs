//! `alba check`: validate the Beamfile without running anything.
//!
//! Loading already happened in `main.rs` by the time this runs — a load
//! failure never reaches here, it is rendered and reported by the caller
//! instead (see `commands` module doc comment). This module only formats
//! the success message.

use alba_core::Project;

use crate::render::LineSink;

/// Prints `✓ Beamfile: N beam(s)` and returns the exit code for a
/// successful check. Always `0`: `main.rs` only calls this once loading
/// has already succeeded, so there is no failure path left to report here.
///
/// Written through a [`LineSink`] rather than `println!`: `alba check |
/// head -1` (or any consumer that closes its end without reading) closes
/// stdout before this single line lands, and `println!` panics on a
/// closed pipe.
pub fn run(project: &Project) -> i32 {
    let count = project.beams.len();
    let noun = if count == 1 { "beam" } else { "beams" };
    LineSink::stdout().line(&format!("\u{2713} Beamfile: {count} {noun}"));
    0
}
