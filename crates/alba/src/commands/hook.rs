//! `alba hook <name> [args]`: what the installed scripts call. The strict
//! equivalent of `alba run <beam> <args>` with the text renderers.

use std::path::Path;

use alba_core::{Project, SourceMap};
use alba_engine::Selection;

use crate::args::{OutputStyle, RunFlags};

pub fn run(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    name: &str,
    args: Vec<String>,
) -> i32 {
    // Undeclared: the script exists for every hook git knows, so this is
    // the common case and must be silent.
    let Some(hook) = project.hooks.iter().find(|hook| hook.name == name) else {
        return 0;
    };
    let arity = project
        .beams
        .iter()
        .find(|beam| beam.id == hook.beam.value)
        .map_or(0, |beam| beam.params.len());
    let mut args = args;
    args.truncate(arity);
    let flags = RunFlags {
        no_ui: true,
        output: Some(OutputStyle::Grouped),
        ..RunFlags::default()
    };
    super::run::run(
        project,
        sources,
        beamfile,
        &Selection::Beam(hook.beam.value.clone()),
        args,
        &flags,
    )
}
