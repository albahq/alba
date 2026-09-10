//! Bare `alba` with no `default` beam declared: lists every beam, sorted
//! by id, one per line as `<id>    <description>`. A beam with no
//! `description` gets a dimmed `(no description)` placeholder instead —
//! dimmed only when `crate::color_enabled()` says color is on (see that
//! function's doc comment for the TTY/`NO_COLOR` detection it centralizes).

use alba_core::Project;
use owo_colors::OwoColorize;

use crate::render::LineSink;

const NO_DESCRIPTION: &str = "(no description)";

/// Prints one line per beam (sorted by id) and returns `0`. Like
/// [`crate::commands::check::run`], loading already happened in `main.rs`
/// by the time this runs, so there is no failure path here.
///
/// Written through a [`LineSink`] rather than `println!`, exactly like the
/// renderers in `crate::render` — `alba | head -1` closes stdout as soon
/// as `head` has its one line, and `println!` panics on that closed pipe.
pub fn run(project: &Project) -> i32 {
    let mut beams: Vec<_> = project.beams.iter().collect();
    beams.sort_by(|a, b| a.id.0.cmp(&b.id.0));

    let mut out = LineSink::stdout();
    for beam in beams {
        match &beam.description {
            Some(description) => out.line(&format!("{}    {description}", beam.id.0)),
            None if crate::color_enabled() => {
                out.line(&format!("{}    {}", beam.id.0, NO_DESCRIPTION.dimmed()))
            }
            None => out.line(&format!("{}    {NO_DESCRIPTION}", beam.id.0)),
        }
    }
    0
}
