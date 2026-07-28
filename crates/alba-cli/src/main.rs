//! The `alba` binary: argument parsing, Beamfile loading, and dispatch to
//! a subcommand.
//!
//! Loading happens exactly once here, before any subcommand runs — see the
//! `commands` module doc comment for why that split exists. A load failure
//! (missing file, parse error, validation error) is rendered and reported
//! from this one place, so every command downstream only has a success
//! path left to implement.

mod args;
mod commands;
mod exit;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use clap::Parser;

use args::{Cli, Command};
use exit::EXIT_ALBA_ERROR;

fn main() {
    let cli = Cli::parse();
    std::process::exit(run(cli));
}

fn run(cli: Cli) -> i32 {
    let beamfile = match resolve_beamfile(cli.file.as_deref()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return EXIT_ALBA_ERROR;
        }
    };

    // `sources` is unused by every command in this task (`check` and bare
    // listing only need `project`); Task 12's `run` will need it to render
    // schedule-time diagnostics, at which point this becomes `sources`.
    let (project, _sources) = match alba_core::load_project(&beamfile) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprint!("{}", render_load_error(err));
            return EXIT_ALBA_ERROR;
        }
    };

    match cli.command {
        Some(Command::Check) => commands::check::run(&project),
        None => match &project.default {
            // TODO(task-12): replace with a real dispatch to `run <target>`.
            // The only contract later tests may rely on until then is that
            // the target's name appears in the output.
            Some(target) => {
                println!("would run `{}`", target.0);
                0
            }
            None => commands::list::run(&project),
        },
    }
}

/// Resolves the Beamfile to load from an optional `--file` value, and
/// reports a missing file as the CLI's own message rather than
/// `alba_core::load_project`'s (which would read
/// `cannot read Beamfile ...: No such file or directory`, an I/O-flavored
/// message meant for a file that *changed* mid-load, not "you never had
/// one").
///
/// `<dir>` in the resulting message is the absolute directory that was
/// searched — computed with `std::path::absolute`, which (like
/// `alba_core::loader`'s own `beam_dir_for`) is purely lexical and never
/// requires the path to exist, so it works for a directory that does not
/// exist either. Without `--file` that is the process's current directory;
/// with `--file some/where/Other.beam` it is `some/where`. The searched
/// *name* is likewise taken from `--file`'s own file name (`Other.beam`)
/// rather than hardcoded to the literal word `Beamfile` — reporting
/// "searched Beamfile" while `--file` named something else entirely would
/// be actively misleading, not just imprecise.
fn resolve_beamfile(file: Option<&Path>) -> Result<PathBuf, String> {
    let (path, dir, name): (PathBuf, PathBuf, String) = match file {
        Some(file) => {
            let dir = file
                .parent()
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| file.display().to_string());
            (file.to_path_buf(), dir, name)
        }
        None => (
            PathBuf::from("Beamfile"),
            PathBuf::from("."),
            "Beamfile".to_string(),
        ),
    };

    if path.is_file() {
        return Ok(path);
    }

    let dir = std::path::absolute(&dir).unwrap_or(dir);
    Err(format!(
        "no Beamfile found in {} (searched {name})",
        dir.display()
    ))
}

/// Renders a [`alba_core::LoadError`] the same way the design intends every
/// spanned failure to look: the source line, a caret under the exact span,
/// and help text, via `alba_syntax::render_diagnostic`. Falls back to the
/// bare message on the (currently unreachable in practice) case where the
/// error's `source_id` was never registered in `sources`.
fn render_load_error(err: alba_core::LoadError) -> String {
    let alba_core::LoadError { error, sources } = err;
    let source_id = error.source_id;
    let diagnostic = error.into_diagnostic();

    match sources.get(source_id) {
        Some((path, source)) => {
            alba_syntax::render_diagnostic(source, &path.display().to_string(), &diagnostic)
        }
        None => format!("{diagnostic}\n"),
    }
}

/// Whether ANSI color should be emitted: stdout is a real terminal, and the
/// user has not opted out via `NO_COLOR` (https://no-color.org). Checked
/// explicitly here rather than left to `owo-colors`' own ambient detection,
/// which does not account for `NO_COLOR` and would otherwise make every
/// `assert_cmd` test in this task (and Task 12's, which reuses this same
/// function for per-beam colours) compare captured output against escape
/// codes on top of whatever it actually asserts.
pub(crate) fn color_enabled() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}
