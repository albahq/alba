//! `alba check`: validate the Beamfile without running anything.
//!
//! Loading already happened in `main.rs` by the time this runs — a load
//! failure never reaches here, it is rendered and reported by the caller
//! instead (see `commands` module doc comment). Beyond that, this is the
//! one place static validation happens *before* a beam ever runs: every
//! embedded-shell beam with no parameters has its `run` templates
//! rendered and parsed the same way [`alba_shell::parse`] would at
//! schedule time, so unsupported syntax is caught here instead of first
//! showing up mid-run.

use alba_core::{ExecutorKind, Project, render_template};

use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

/// Validates every beam that can be validated statically, then prints
/// `✓ Beamfile: N beam(s)` and returns `0` — or, if any beam's embedded
/// shell command failed to parse, prints that beam's diagnostic instead
/// and returns [`EXIT_ALBA_ERROR`] without the success line.
///
/// Only beams running on the embedded shell (`ExecutorKind::Shell`, the
/// default) with no parameters are checked: a parameterized beam's `run`
/// template cannot be rendered until its arguments arrive, and
/// `system_shell`/`docker`/plugin beams do not speak this grammar at all
/// (the `!= ExecutorKind::Shell` filter below covers plugin beams too, with
/// no separate case needed). A template that fails to render here is
/// skipped silently — rendering failures are load's concern, and load
/// already checked everything that can be checked without parameters.
///
/// Written through a [`LineSink`] rather than `println!`/`eprintln!`:
/// `alba check | head -1` (or any consumer that closes its end without
/// reading) closes the stream before every line lands, and the macros
/// panic on a closed pipe.
pub fn run(project: &Project) -> i32 {
    let mut err = LineSink::stderr();
    let mut failed = false;

    for beam in &project.beams {
        if beam.executor != ExecutorKind::Shell || !beam.params.is_empty() {
            continue;
        }
        for template in &beam.run {
            let Ok(command) = render_template(template, &beam.scope) else {
                continue;
            };
            if let Err(error) = alba_shell::parse(&command) {
                failed = true;
                err.line(&format!(
                    "beam `{}`: invalid embedded shell command",
                    beam.id.0
                ));
                for line in error.render(&command).lines() {
                    err.line(line);
                }
            }
        }
    }

    if failed {
        return EXIT_ALBA_ERROR;
    }

    let count = project.beams.len();
    let noun = if count == 1 { "beam" } else { "beams" };
    LineSink::stdout().line(&format!("\u{2713} Beamfile: {count} {noun}"));
    0
}
