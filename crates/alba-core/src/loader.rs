//! [`load_project`]: the single public entry point onto this crate. Loads
//! a root Beamfile and every file it (transitively) `import`s, namespacing
//! each imported beam's id and `needs` entries by the alias it was
//! imported under, and stamping every beam with the `SourceId` and `dir`
//! (its defining file's directory) of the file that declared it.
//!
//! ## Resolution rules
//!
//! - `import "api/Beamfile" as api` resolves the path relative to the
//!   *importing file's* directory, not the process's current directory.
//! - A beam declared in an imported file gets its id (and every entry of
//!   its `needs` list) prefixed with `alias:`. A `needs [build]` entry
//!   local to the imported file stays file-local — it's namespaced to
//!   `alias:build` by the same prefixing, exactly like the beam it refers
//!   to. `needs [other_alias:build]`, referring to one of *that file's
//!   own* imports, is namespaced the same way (matching
//!   `alba_syntax::BeamRef`'s one-level `namespace` field). Nested imports
//!   compose: a beam reached through two levels of aliasing ends up with a
//!   two-segment id (`api:db:migrate`).
//! - `default` is read only from the root file; an imported file's
//!   `default` is parsed (so it's still a valid Beamfile on its own) but
//!   never propagated.
//! - `version` is parsed (by `alba_syntax`) but not semantically used
//!   anywhere in this crate, in the root file or an imported one — this
//!   matches single-file loading's pre-existing behavior (Task 6 never
//!   validated it either), so imported files are treated no differently
//!   from the root on this point.
//! - An import alias can never contain `:`: it's parsed as a single
//!   identifier token (`alba_syntax`'s `eat_ident`, the same rule beam and
//!   parameter names follow), so `import "x" as a:b` is already a syntax
//!   error before this crate ever sees it. No additional validation is
//!   needed here.
//! - Two imports in the same file cannot share an alias — an ambiguity
//!   this crate rejects explicitly (see [`check_duplicate_aliases`]).
//! - Import cycles are detected by canonicalized path (see
//!   [`Loader::load_file`]'s doc comment for why that's safe on
//!   case-insensitive filesystems).
//!
//! ## Two path forms, never mixed
//!
//! Every file this loader touches has two distinct path forms in play,
//! and they are kept strictly separate:
//!
//! - The **display path**: whatever was written or joined lexically —
//!   `root` exactly as the caller passed it, or an import's path joined
//!   onto its importer's display path with a plain [`Path::join`], never
//!   touching the filesystem. This is the *only* form ever stored in
//!   [`SourceMap`], used to compute `dir` (the join base for further
//!   imports and a beam's `cwd` base), or shown in a message.
//! - The **canonical path**: `std::fs::canonicalize`'s output. Used
//!   *exclusively* as the identity key on [`Loader::stack`] for cycle
//!   detection — never stored, joined onto, or displayed.
//!
//! Mixing them is the bug this split exists to prevent: on Windows,
//! `canonicalize` returns a verbatim (`\\?\`) path, and Rust's standard
//! library passes verbatim paths to Win32 completely unnormalized — no
//! `.` or `..` resolution, no forward-slash-to-backslash conversion.
//! Joining a further relative import path onto a canonical `dir` would
//! silently produce an unreadable path on Windows even though every path
//! segment involved is individually valid. Display paths are always plain
//! (non-verbatim) paths built with ordinary [`Path::join`], so this can't
//! happen — the actual filesystem access that resolves `.`/`..`/symlinks
//! happens once, implicitly, when the OS opens the file for reading.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use alba_syntax::{File, Span};

use crate::error::{CoreError, SourceIdScope};
use crate::eval::{build_project, parse_error_to_core_error};
use crate::model::{Beam, BeamId, Project, SourceId};

/// The path and source text of every file [`load_project`] read, indexed
/// by [`SourceId`], so a [`CoreError`]'s `source_id` can be turned back
/// into something renderable: look up `(path, source)` here and hand both
/// to `alba_syntax::render_diagnostic(source, &path.display().to_string(),
/// &diagnostic)`.
#[derive(Debug, Default, Clone)]
pub struct SourceMap {
    entries: Vec<(PathBuf, String)>,
}

impl SourceMap {
    fn push(&mut self, path: PathBuf, source: String) -> SourceId {
        let id = SourceId(self.entries.len());
        self.entries.push((path, source));
        id
    }

    /// The path and source text of the file `id` was assigned to.
    /// `None` only when `id` was never registered — the sole way that
    /// happens is [`load_project`] failing to read the root file itself,
    /// before any entry exists to assign an id to.
    pub fn get(&self, id: SourceId) -> Option<(&Path, &str)> {
        self.entries
            .get(id.0)
            .map(|(path, source)| (path.as_path(), source.as_str()))
    }
}

/// A [`load_project`] failure, paired with every source file successfully
/// read before it happened. `sources` lets a caller render `error` (via
/// [`SourceMap::get`] and `alba_syntax::render_diagnostic`) even though
/// loading never finished — `error.source_id` is always resolvable
/// through it, since a file is registered in `sources` before it's parsed
/// or evaluated.
///
/// Implements [`std::error::Error`] (via `thiserror`, `#[source]`-linked to
/// `error`) so it composes with `?` into `Box<dyn Error>`/`anyhow`, not
/// just with this crate's own `Result<_, LoadError>`.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct LoadError {
    #[source]
    pub error: CoreError,
    pub sources: SourceMap,
}

/// Loads `root` and every Beamfile it (transitively) `import`s into one
/// [`Project`], namespacing imported beams by the alias they were
/// imported under. See the module doc comment for the resolution rules.
///
/// This is the single public entry point the CLI calls: `load_str` (used
/// internally by this crate's and the engine's tests) stays
/// `#[doc(hidden)]`.
pub fn load_project(root: &Path) -> Result<(Project, SourceMap), LoadError> {
    let mut loader = Loader::default();
    match loader.load_file(root, Span::new(0, 0)) {
        Ok((beams, default)) => Ok((Project { beams, default }, loader.sources)),
        Err(error) => Err(LoadError {
            error,
            sources: loader.sources,
        }),
    }
}

#[derive(Default)]
struct Loader {
    sources: SourceMap,
    /// `(canonical path, display path)` pairs for the files currently on
    /// the DFS chain from the root to whichever file is being loaded right
    /// now. Only the canonical half is ever compared (to detect an import
    /// cycle); the display half is what a cycle's error message actually
    /// shows. Not used to memoize already-finished files — two different
    /// import sites can legitimately load the same file under different
    /// aliases, producing differently-namespaced beams each time.
    stack: Vec<(PathBuf, PathBuf)>,
}

impl Loader {
    /// Resolves and reads `path` (a display path — see the module doc
    /// comment), reporting a failure at `error_span` (stamped with
    /// whichever file is currently loading — the importer, for every call
    /// except the very first).
    ///
    /// Returns the *canonical* form alongside the source text: canonicalizing
    /// is what makes cycle detection in [`Loader::load_file`] correct — on a
    /// case-insensitive filesystem (the default on macOS and Windows),
    /// `std::fs::canonicalize` normalizes a path to the casing actually on
    /// disk, so two imports spelling the same file with different casing
    /// still compare equal on the `stack`. That canonical path is used for
    /// nothing else: reading goes through `path` as given, and the caller
    /// never derives `dir` or a [`SourceMap`] entry from the canonical
    /// form (see the module doc comment for why).
    fn resolve_and_read(path: &Path, error_span: Span) -> Result<(PathBuf, String), CoreError> {
        let not_found = |e: std::io::Error| {
            CoreError::new(
                format!("cannot read Beamfile `{}`: {e}", path.display()),
                error_span,
            )
        };
        let source = std::fs::read_to_string(path).map_err(not_found)?;
        let canonical = std::fs::canonicalize(path).map_err(not_found)?;
        Ok((canonical, source))
    }

    /// Recursively loads `path` (a display path: already resolved,
    /// lexically, relative to whatever imported it — or the root path the
    /// caller passed to [`load_project`]) and everything it imports in
    /// turn, returning that subtree's beams — ids and `needs` namespaced
    /// relative to `path` itself — plus `path`'s own `default` (the caller
    /// decides whether that's meaningful: only [`load_project`]'s
    /// top-level call uses it, every recursive call for an `import`
    /// discards it).
    ///
    /// `error_span` is where a failure to read `path`, or an import cycle
    /// closing on it, gets reported; it's the span of the `import` that
    /// named `path` for every call except the very first (the root has no
    /// such span, so [`load_project`] passes a zero-width one).
    fn load_file(
        &mut self,
        path: &Path,
        error_span: Span,
    ) -> Result<(Vec<Beam>, Option<BeamId>), CoreError> {
        let (canonical, source) = Self::resolve_and_read(path, error_span)?;

        if let Some(start) = self.stack.iter().position(|(c, _)| *c == canonical) {
            let mut chain: Vec<String> = self.stack[start..]
                .iter()
                .map(|(_, display)| display.display().to_string())
                .collect();
            chain.push(path.display().to_string());
            return Err(CoreError::new(
                format!("import cycle: {}", chain.join(" -> ")),
                error_span,
            ));
        }

        // `dir` is derived from `path` (the display form), never from
        // `canonical` — see the module doc comment. `path.parent()` for a
        // bare filename like `"Beamfile"` (no directory component) is
        // `Some("")`, and joining onto an empty `PathBuf` is a no-op, so
        // this still resolves further imports relative to the process's
        // current directory in that case, matching the display path's own
        // (relative) frame of reference.
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let source_id = self.sources.push(path.to_path_buf(), source.clone());

        self.stack.push((canonical, path.to_path_buf()));
        let result = self.load_file_body(&source, &dir, source_id);
        self.stack.pop();
        result
    }

    /// The part of [`Loader::load_file`] that runs once `path` has been
    /// read and pushed onto the cycle-detection stack: parse, evaluate
    /// this file's own beams, check for a duplicate import alias, then
    /// recurse into each import and namespace its beams. Split out so
    /// `load_file` can guarantee `stack.pop()` runs via a plain
    /// `?`-propagating helper rather than a closure.
    fn load_file_body(
        &mut self,
        source: &str,
        dir: &Path,
        source_id: SourceId,
    ) -> Result<(Vec<Beam>, Option<BeamId>), CoreError> {
        let _scope = SourceIdScope::enter(source_id);
        let file: File = alba_syntax::parse(source).map_err(parse_error_to_core_error)?;
        check_duplicate_aliases(&file)?;

        let local = build_project(&file, dir)?;
        let mut beams = local.beams;

        for import in &file.imports {
            let import_path = dir.join(&import.path.value);
            let (mut child_beams, _child_default) =
                self.load_file(&import_path, import.path.span)?;
            let alias = &import.alias.value;
            for beam in &mut child_beams {
                beam.id.0 = format!("{alias}:{}", beam.id.0);
                for need in &mut beam.needs {
                    need.0 = format!("{alias}:{}", need.0);
                }
            }
            beams.extend(child_beams);
        }

        Ok((beams, local.default))
    }
}

/// Rejects two `import`s in the same file sharing an alias — otherwise
/// namespace prefixing would silently merge two unrelated files' beams
/// under the same prefix.
fn check_duplicate_aliases(file: &File) -> Result<(), CoreError> {
    let mut seen = HashSet::new();
    for import in &file.imports {
        if !seen.insert(import.alias.value.as_str()) {
            return Err(CoreError::new(
                format!("duplicate import alias `{}`", import.alias.value),
                import.alias.span,
            ));
        }
    }
    Ok(())
}
