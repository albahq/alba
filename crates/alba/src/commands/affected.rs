//! `alba affected [ref]`: list the beams a git diff affects, run nothing.
//! Without a reference, the diff is from the `base` the Beamfile declares.

use alba_core::{Project, SourceMap};
use std::path::Path;

use crate::args::LogFormat;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

pub fn run(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    reference: Option<&str>,
    log_format: LogFormat,
) -> i32 {
    // The list is the selection `--affected` would run, so the reference
    // follows the same rule: given, or declared.
    let source = super::run::given_or_declared(reference);
    let Some(reference) = source.reference(project) else {
        LineSink::stderr().line(&super::run::missing_base(project, "`alba affected <REF>`"));
        return EXIT_ALBA_ERROR;
    };
    let root = alba_engine::beamfile_dir(beamfile);
    let affected = match alba_engine::affected(project, sources, &root, reference) {
        Ok(affected) => affected,
        Err(error) => {
            LineSink::stderr().line(&error.to_string());
            return EXIT_ALBA_ERROR;
        }
    };
    let mut out = LineSink::stdout();
    match log_format {
        LogFormat::Json => {
            let beams: Vec<&str> = affected.iter().map(|id| id.0.as_str()).collect();
            out.line(&serde_json::json!({ "beams": beams }).to_string());
        }
        LogFormat::Text => {
            for id in &affected {
                let parameterized = project
                    .beams
                    .iter()
                    .any(|beam| beam.id == *id && !beam.params.is_empty());
                if parameterized {
                    out.line(&format!("{} (takes parameters)", id.0));
                } else {
                    out.line(&id.0);
                }
            }
        }
    }
    0
}
