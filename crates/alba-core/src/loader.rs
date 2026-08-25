//! [`load_project`]: the single public entry point onto this crate. Loads
//! a root Beamfile and every file it (transitively) `import`s, namespacing
//! each imported beam's id and `needs` entries by the alias it was
//! imported under, and stamping every beam with the `SourceId` of the file
//! that declared it and a `dir` (its defining file's directory, always
//! absolute — see "Three path forms, never mixed" below) used as the
//! `cwd` base.
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
//!   own* imports, is namespaced the same way. Nested imports compose: a
//!   beam reached through two levels of aliasing ends up with a
//!   two-segment id (`api:db:migrate`), and `needs` can spell as many
//!   segments as a reference needs.
//! - `default` is read only from the root file; an imported file's
//!   `default` is parsed (so it's still a valid Beamfile on its own) but
//!   never propagated.
//! - `version` is parsed (by `alba_syntax`) but not semantically used
//!   anywhere in this crate, in the root file or an imported one — this
//!   matches single-file loading's behavior, so imported files are
//!   treated no differently from the root on this point.
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
//! ## Three path forms, never mixed
//!
//! Every file this loader touches has three distinct path forms in play,
//! each with exactly one job, and they are kept strictly separate:
//!
//! - The **display path**: whatever was written or joined lexically —
//!   `root` exactly as the caller passed it, or an import's path joined
//!   onto its *importer's own display path* with a plain [`Path::join`],
//!   never touching the filesystem and never absolutized. This is the
//!   *only* form ever stored in [`SourceMap`] or shown in a message (a
//!   missing-import error, an import cycle's chain) — so what a user sees
//!   in a diagnostic is always traceable back to what they (or an
//!   `import` statement) actually typed, never a path this loader
//!   invented.
//! - The **canonical path**: `std::fs::canonicalize`'s output. Used
//!   *exclusively* as the identity key on [`Loader::stack`] for cycle
//!   detection — never stored, joined onto, or displayed.
//! - The **beam directory** (`beam_dir` in [`Loader::load_file`]):
//!   `std::path::absolute`'s output, used *exclusively* to become
//!   `Beam::dir` (the `cwd` base a later engine hands to a process spawn,
//!   possibly long after loading finished and the process's current
//!   directory may have changed). Computed independently from the display
//!   path for each file — never joined onto to build another file's
//!   display path, and never itself absolutized further by a child's
//!   `beam_dir` (each file's `beam_dir` comes straight from its own
//!   `path`, not from its importer's `beam_dir`).
//!
//! Mixing the display path with either of the other two is exactly the
//! bug each split exists to prevent:
//!
//! - Display vs. canonical: on Windows, `canonicalize` returns a verbatim
//!   (`\\?\`) path, and Rust's standard library passes verbatim paths to
//!   Win32 completely unnormalized — no `.`/`..` resolution, no
//!   forward-slash-to-backslash conversion. Joining a further relative
//!   import path onto a canonicalized directory would silently produce an
//!   unreadable path on Windows even though every path segment involved
//!   is individually valid.
//! - Display vs. beam directory: joining onto an *absolutized* directory
//!   instead of the display path would make every descendant's display
//!   path (and `SourceMap` entry) absolute too, even when the user typed
//!   a relative `root` — silently showing them a path they never wrote.
//!   `std::path::absolute` itself stays safe to use for `beam_dir`
//!   precisely because it's *not* threaded into the display-path chain:
//!   unlike `canonicalize`, it's purely lexical (it does not touch the
//!   filesystem or require the path to exist) and, per
//!   `library/std/src/sys/path/windows.rs`'s `absolute` implementation,
//!   it only returns a path unchanged (skipping normalization) when the
//!   *input* is already verbatim — our inputs never are, since they're
//!   always built by joining onto a display path, never a canonicalized
//!   one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alba_syntax::{File, Span, Spanned};

use crate::error::{CoreError, SourceIdScope};
use crate::eval::{LazyGit, build_project, parse_error_to_core_error};
use crate::model::{Beam, BeamId, Hook, Project, SourceId};

/// The path and source text of every file [`load_project`] read, indexed
/// by [`SourceId`], so a [`CoreError`]'s `source_id` can be turned back
/// into something renderable: look up `(path, source)` here and hand both
/// to `alba_syntax::render_diagnostic(source, &path.display().to_string(),
/// &diagnostic)`.
///
/// Two maps are equal when they registered the same files with the same
/// text — which is to say when loading found the project unchanged. A
/// caller that reloads repeatedly (the watch session, on every batch its
/// watcher delivers) uses that to tell a reload that changed something
/// from one that only cost a file read: loading is deterministic, so equal
/// sources mean an equal outcome, down to the rendered diagnostic.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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

    /// The path of every file this map registered, in [`SourceId`] order:
    /// the root Beamfile first, then each import in load order. This is the
    /// watch loop's source of truth for which Beamfiles to put under watch.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.entries.iter().map(|(path, _)| path.as_path())
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
///
/// Runs [`crate::graph::validate_graph`] once every file has finished
/// loading, before returning the assembled [`Project`] — that check runs
/// with no [`SourceIdScope`] active (loading is over by then), so it
/// stamps each error it produces with the offending beam's own `source`
/// field itself, rather than leaving it at whatever this thread's ambient
/// id defaults to.
pub fn load_project(root: &Path) -> Result<(Project, SourceMap), LoadError> {
    let mut loader = Loader::default();
    match loader.load_file(root, Span::new(0, 0)) {
        Ok((beams, default, hooks)) => {
            let project = Project {
                beams,
                default,
                hooks,
            };
            match crate::graph::validate_graph(&project) {
                Ok(()) => Ok((project, loader.sources)),
                Err(error) => Err(LoadError {
                    error,
                    sources: loader.sources,
                }),
            }
        }
        Err(error) => Err(LoadError {
            error,
            sources: loader.sources,
        }),
    }
}

/// What one file's load produced: its own beams and everything it
/// imports, ids and `needs` namespaced relative to that file itself, plus
/// its own `default` and `hooks`.
type LoadedFile = (Vec<Beam>, Option<Spanned<BeamId>>, Vec<Hook>);

#[derive(Default)]
struct Loader {
    sources: SourceMap,
    /// `(canonical path, display path)` pairs for the files currently on
    /// the DFS chain from the root to whichever file is being loaded right
    /// now. Only the canonical half is ever compared (to detect an import
    /// cycle); the display half is what a cycle's error message actually
    /// shows.
    stack: Vec<(PathBuf, PathBuf)>,
    /// Every file already loaded to completion, keyed by canonical path.
    ///
    /// Two import sites can legitimately reach the same file under
    /// different aliases and must end up with differently-namespaced
    /// beams — but that difference is applied *afterwards*, by the
    /// importing file, and is purely textual. Reading, parsing, and
    /// evaluating the file itself is identical either way, so it happens
    /// once. Without this, cost is exponential in nesting depth: a chain
    /// of files that each import the next twice doubles the work per
    /// level, and nineteen three-line files were enough to make loading
    /// take half a minute.
    ///
    /// A file on `stack` is by definition not in here (nothing is recorded
    /// until its load returns), so memoization can never mask an import
    /// cycle. The [`SourceMap`] entry, and therefore what a diagnostic
    /// about that file shows, comes from whichever import site reached it
    /// first — the display path the *other* site would have produced names
    /// the same file, so both point a reader at the same text.
    loaded: HashMap<PathBuf, LoadedFile>,
    /// The one `LazyGit` shared by every file this load reads, rooted at
    /// the first Beamfile's directory (the root, since it is the first
    /// file [`Loader::load_file`] ever sees). Lazily created on that first
    /// call so a `load_project` that never even opens a file spawns
    /// nothing.
    git: Option<Arc<LazyGit>>,
}

impl Loader {
    /// The canonical form of `path` (a display path — see the module doc
    /// comment), reporting a failure at `error_span` (stamped with
    /// whichever file is currently loading — the importer, for every call
    /// except the very first).
    ///
    /// Canonicalizing is what makes cycle detection and memoization in
    /// [`Loader::load_file`] correct — on a case-insensitive filesystem
    /// (the default on macOS and Windows), `std::fs::canonicalize`
    /// normalizes a path to the casing actually on disk, so two imports
    /// spelling the same file with different casing still compare equal.
    /// The canonical path is used for nothing else: reading goes through
    /// `path` as given, and the caller never derives `dir` or a
    /// [`SourceMap`] entry from the canonical form (see the module doc
    /// comment for why).
    fn canonical(path: &Path, error_span: Span) -> Result<PathBuf, CoreError> {
        std::fs::canonicalize(path).map_err(|e| unreadable(path, &e, error_span))
    }

    /// Reads `path` as it was written. Split from [`Loader::canonical`] so
    /// a file already loaded through another import site can be recognized
    /// before it is read a second time.
    fn read(path: &Path, error_span: Span) -> Result<String, CoreError> {
        std::fs::read_to_string(path).map_err(|e| unreadable(path, &e, error_span))
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
    /// A file already loaded through another import site is returned from
    /// [`Loader::loaded`] rather than read again — see that field's doc
    /// comment.
    ///
    /// `error_span` is where a failure to read `path`, or an import cycle
    /// closing on it, gets reported; it's the span of the `import` that
    /// named `path` for every call except the very first (the root has no
    /// such span, so [`load_project`] passes a zero-width one).
    fn load_file(&mut self, path: &Path, error_span: Span) -> Result<LoadedFile, CoreError> {
        let canonical = Self::canonical(path, error_span)?;

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

        if let Some(loaded) = self.loaded.get(&canonical) {
            return Ok(loaded.clone());
        }

        let source = Self::read(path, error_span)?;

        // `import_base` (a display path) is derived from `path`, never
        // from `canonical` — see the module doc comment. `path.parent()`
        // for a bare filename like `"Beamfile"` (no directory component)
        // is `Some("")`, not `None`, so the `unwrap_or_else` below rarely
        // fires in practice — but joining onto an empty `PathBuf` is a
        // harmless no-op, so this still resolves further imports relative
        // to the process's current directory in that case. Used *only* to
        // build each import's display path (`import_path` below); it is
        // never what becomes `Beam::dir` (see `beam_dir`).
        let import_base = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let beam_dir = beam_dir_for(path, error_span)?;
        let git = Arc::clone(
            self.git
                .get_or_insert_with(|| Arc::new(LazyGit::new(beam_dir.clone()))),
        );

        let source_id = self.sources.push(path.to_path_buf(), source.clone());

        self.stack.push((canonical.clone(), path.to_path_buf()));
        let result = self.load_file_body(&source, &import_base, &beam_dir, source_id, git);
        self.stack.pop();

        let loaded = result?;
        self.loaded.insert(canonical, loaded.clone());
        Ok(loaded)
    }

    /// The part of [`Loader::load_file`] that runs once `path` has been
    /// read and pushed onto the cycle-detection stack: parse, evaluate
    /// this file's own beams (stamped with `beam_dir`), check for a
    /// duplicate import alias, then recurse into each import (joined onto
    /// `import_base`) and namespace its beams. Split out so `load_file`
    /// can guarantee `stack.pop()` runs via a plain `?`-propagating helper
    /// rather than a closure.
    fn load_file_body(
        &mut self,
        source: &str,
        import_base: &Path,
        beam_dir: &Path,
        source_id: SourceId,
        git: Arc<LazyGit>,
    ) -> Result<LoadedFile, CoreError> {
        let _scope = SourceIdScope::enter(source_id);
        let file: File = alba_syntax::parse(source).map_err(parse_error_to_core_error)?;
        check_duplicate_aliases(&file)?;

        let local = build_project(&file, beam_dir, git)?;
        let mut beams = local.beams;

        for import in &file.imports {
            let import_path = import_base.join(&import.path.value);
            let (mut child_beams, _child_default, _child_hooks) =
                self.load_file(&import_path, import.path.span)?;
            let alias = &import.alias.value;
            for beam in &mut child_beams {
                beam.id.0 = format!("{alias}:{}", beam.id.0);
                for need in &mut beam.needs {
                    need.value.0 = format!("{alias}:{}", need.value.0);
                }
            }
            beams.extend(child_beams);
        }

        Ok((beams, local.default, local.hooks))
    }
}

/// The single "cannot read this Beamfile" message, shared by the two
/// filesystem calls [`Loader::load_file`] makes on a file's path, so the
/// two cannot drift into describing the same missing file differently.
fn unreadable(path: &Path, error: &std::io::Error, error_span: Span) -> CoreError {
    CoreError::new(
        format!("cannot read Beamfile `{}`: {error}", path.display()),
        error_span,
    )
}

/// Computes the directory that becomes `Beam::dir` for every beam declared
/// in the file at `path`: `std::path::absolute(path)`'s parent. Reported
/// at `error_span` if `path` can't be absolutized (rare — see
/// `std::path::absolute`'s docs for when it fails; an empty `path` is the
/// only realistic case, and `path` here is never empty).
///
/// Deliberately a free function taking a `&Path` rather than a
/// `Loader` method: it touches neither `self` nor the filesystem
/// (`std::path::absolute` does not require `path` to exist), which is
/// what lets it be unit tested directly against synthetic paths — a bare
/// filename, a relative path, an already-absolute one — without a real
/// file, a `tempdir`, or (worse) mutating the process's current directory
/// to fabricate a "relative root" scenario, which would race every other
/// test in this crate's suite (`cargo test` runs test functions
/// concurrently on a shared process).
fn beam_dir_for(path: &Path, error_span: Span) -> Result<PathBuf, CoreError> {
    let absolute = std::path::absolute(path).map_err(|e| {
        CoreError::new(
            format!(
                "cannot resolve an absolute path for `{}`: {e}",
                path.display()
            ),
            error_span,
        )
    })?;
    Ok(absolute
        .parent()
        .map(Path::to_path_buf)
        // Unreachable in practice: `std::path::absolute` keeps `path`'s
        // final component (the Beamfile's own file name), so the result
        // always has a parent — even a filesystem-root file (`/Beamfile`)
        // absolutizes to `/Beamfile`, whose parent is `/`, itself a valid
        // absolute directory (see this function's tests). Kept as a
        // non-panicking fallback rather than `.expect(..)`: this is a
        // library, and a defensive branch should never crash the caller
        // even if the "unreachable" reasoning turns out wrong on some
        // future platform.
        .unwrap_or_else(|| PathBuf::from(".")))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the fix-round bug where `Beam::dir` for a
    /// bare-filename root (`load_project(Path::new("Beamfile"))`, a
    /// plausible CLI default) was the *empty* path: `Path::new("Beamfile")
    /// .parent()` is `Some("")`, not `None`, so a fallback keyed on `None`
    /// never fired. `beam_dir_for` must never produce that: it's absolute
    /// and non-empty here.
    #[test]
    fn beam_dir_for_bare_filename_is_absolute_and_non_empty() {
        let dir = beam_dir_for(Path::new("Beamfile"), Span::new(0, 0)).unwrap();
        assert!(dir.is_absolute());
        assert!(!dir.as_os_str().is_empty());
        assert!(!dir.to_string_lossy().starts_with(r"\\?\"));
    }

    /// A relative root with a directory component (`load_project(Path::new(
    /// "sub/Beamfile"))`) must still resolve to an absolute `Beam::dir` —
    /// otherwise it's only correct for as long as the process's current
    /// directory doesn't change between loading and running, a coupling
    /// the previous (`canonicalize`-based) implementation didn't have.
    #[test]
    fn beam_dir_for_relative_path_is_absolute() {
        let dir = beam_dir_for(Path::new("sub/Beamfile"), Span::new(0, 0)).unwrap();
        assert!(dir.is_absolute());
        assert!(dir.ends_with("sub"));
        assert!(!dir.to_string_lossy().starts_with(r"\\?\"));
    }

    /// An already-absolute root stays absolute and gains no verbatim
    /// prefix — `std::path::absolute` only returns a path unchanged
    /// (skipping its own normalization) when the *input* is already
    /// verbatim, which nothing in this crate ever constructs (see the
    /// module doc comment's "beam directory" bullet).
    #[test]
    fn beam_dir_for_absolute_path_has_no_verbatim_prefix() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\projects\x\Beamfile")
        } else {
            PathBuf::from("/projects/x/Beamfile")
        };
        let dir = beam_dir_for(&root, Span::new(0, 0)).unwrap();
        assert!(dir.is_absolute());
        assert!(!dir.to_string_lossy().starts_with(r"\\?\"));
    }

    /// A root whose parent is the filesystem root itself (`/Beamfile` on
    /// Unix) still yields a valid, absolute, non-empty directory (`/`) —
    /// the one case where `Path::parent()` could plausibly be `None`
    /// doesn't arise here because `std::path::absolute` always keeps the
    /// file name component.
    #[test]
    fn beam_dir_for_root_level_file_is_still_a_valid_directory() {
        let root = if cfg!(windows) {
            PathBuf::from(r"C:\Beamfile")
        } else {
            PathBuf::from("/Beamfile")
        };
        let dir = beam_dir_for(&root, Span::new(0, 0)).unwrap();
        assert!(dir.is_absolute());
        assert!(!dir.as_os_str().is_empty());
        assert!(!dir.to_string_lossy().starts_with(r"\\?\"));
    }

    /// The watch loop needs every loaded Beamfile path — root and imports —
    /// to put them under watch; `paths()` yields them in load order.
    #[test]
    fn source_map_lists_every_loaded_file_in_load_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("api")).unwrap();
        std::fs::write(
            dir.path().join("Beamfile"),
            "import \"api/Beamfile\" as api\nbeam build { run \"echo root\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("api").join("Beamfile"),
            "beam build { run \"echo api\" }\n",
        )
        .unwrap();

        let (_, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

        let paths: Vec<_> = sources.paths().collect();
        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("Beamfile"));
        assert!(paths[1].ends_with(std::path::Path::new("api").join("Beamfile")));
    }
}
