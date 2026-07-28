//! `alba check`: validate the Beamfile without running anything.
//!
//! Loading already happened in `main.rs` by the time this runs — a load
//! failure never reaches here, it is rendered and reported by the caller
//! instead (see `commands` module doc comment). This module only formats
//! the success message.

use alba_core::Project;

/// Prints `✓ Beamfile: N beam(s)` and returns the exit code for a
/// successful check. Always `0`: `main.rs` only calls this once loading
/// has already succeeded, so there is no failure path left to report here.
pub fn run(project: &Project) -> i32 {
    let count = project.beams.len();
    let noun = if count == 1 { "beam" } else { "beams" };
    println!("\u{2713} Beamfile: {count} {noun}");
    0
}
