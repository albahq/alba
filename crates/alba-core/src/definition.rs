//! Where each beam is written and the commands it runs, as the author
//! wrote them. The evaluated [`Project`] holds rendered or evaluated
//! values; the interface wants the source text, so this reads it back from
//! the [`SourceMap`] through each span.

use std::collections::HashMap;
use std::path::{Component, Path};

use alba_syntax::{Span, line_col};

use crate::files::lexically_normal;
use crate::loader::SourceMap;
use crate::model::{BeamId, Project};

/// A beam as written: its `<file relative to the root>:<line>` and its
/// `run` templates unrendered, quotes removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeamDefinition {
    pub location: String,
    pub commands: Vec<String>,
}

/// The definition of every beam of `project` whose source file is in
/// `sources`. A beam whose file is missing from the map has no entry,
/// rather than an invented location.
///
/// `root` must be spelled the way the Beamfile was loaded (the same base
/// the loader joined its paths onto), since paths are compared lexically.
/// A source outside `root` keeps its full path.
pub fn definitions(
    project: &Project,
    sources: &SourceMap,
    root: &Path,
) -> HashMap<BeamId, BeamDefinition> {
    let root = lexically_normal(root);
    project
        .beams
        .iter()
        .filter_map(|beam| {
            let (path, source) = sources.get(beam.source)?;
            let path = lexically_normal(path);
            // Lexical on purpose, unlike the watch session's `relative_to`:
            // that one resolves symlinks for paths that may no longer exist,
            // while every path here was just read by the loader.
            let relative = path.strip_prefix(&root).unwrap_or(&path);
            let (line, _) = line_col(source, beam.span.start);
            let commands = beam
                .run
                .iter()
                .map(|template| as_written(source, template.span))
                .collect();
            Some((
                beam.id.clone(),
                BeamDefinition {
                    location: format!("{}:{line}", slash_separated(relative)),
                    commands,
                },
            ))
        })
        .collect()
}

/// The source text under `span` minus its surrounding quotes. Escapes and
/// `{interpolations}` stay as typed. A span outside the source yields an
/// empty string.
fn as_written(source: &str, span: Span) -> String {
    let text = source.get(span.start..span.end).unwrap_or_default();
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
}

/// `path` with `/` between its components on every platform, so the
/// location reads the same everywhere.
fn slash_separated(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        match component {
            Component::RootDir => out.push('/'),
            other => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(&other.as_os_str().to_string_lossy());
            }
        }
    }
    out
}
