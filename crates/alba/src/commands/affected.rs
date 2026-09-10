//! `alba affected <ref>`: list the beams a git diff affects, run nothing.

use alba_core::{Project, SourceMap};
use std::path::Path;

use crate::args::LogFormat;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

pub fn run(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    reference: &str,
    log_format: LogFormat,
) -> i32 {
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
