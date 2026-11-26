//! The lists `git.changes(glob)` and `.dirs(glob)` produce, and how a list
//! is quoted when rendered into a command.
//!
//! Everything here is evaluation: [`crate::eval`] validates the calls at
//! load time (where `git.changes()` may appear, that its glob is one of
//! the beam's `inputs`, that a `.dirs()` glob compiles) and calls into
//! this module once a beam renders its `run` and `env`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use alba_syntax::Span;

use crate::error::CoreError;
use crate::files::{
    anchor_pattern, compile_segment_glob, lexical_components, lexically_normal, pattern_dir,
    relative_name,
};
use crate::model::{ChangeMode, ChangeSet, FullReason, Quoting};

/// What `git.changes()` needs to answer for one beam: the run's change
/// set, the beam's Beamfile directory its globs are anchored to, and the
/// directory its commands run in, which the listed paths are relative to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BeamChanges {
    mode: ChangeMode,
    changed: Arc<Vec<PathBuf>>,
    beam_dir: PathBuf,
    cwd: PathBuf,
}

/// What the `git.changes()` calls one scope rendered listed: whether any
/// was reached, and whether any listed a path. Atomic, because a beam's
/// scope travels to the task that renders it.
#[derive(Debug, Default)]
pub(crate) struct CallRecord {
    reached: AtomicBool,
    listed: AtomicBool,
}

impl CallRecord {
    /// Records one evaluated call, which listed nothing when `empty`.
    pub(crate) fn record(&self, empty: bool) {
        self.reached.store(true, Ordering::Relaxed);
        if !empty {
            self.listed.store(true, Ordering::Relaxed);
        }
    }

    /// At least one call was reached, and none listed a path.
    pub(crate) fn listed_nothing(&self) -> bool {
        self.reached.load(Ordering::Relaxed) && !self.listed.load(Ordering::Relaxed)
    }
}

/// A record of what was rendered, not part of what a scope means: two
/// scopes binding the same values are equal whatever they rendered.
impl PartialEq for CallRecord {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl BeamChanges {
    pub(crate) fn new(changes: ChangeSet, beam_dir: &Path, cwd: &Path) -> Self {
        Self {
            mode: changes.mode,
            changed: changes.changed,
            beam_dir: lexically_normal(beam_dir),
            cwd: lexically_normal(cwd),
        }
    }

    /// Full mode for want of a reference, as a beam renders before the
    /// engine has computed a change set: the glob's base directory.
    pub(crate) fn without_reference(beam_dir: &Path, cwd: &Path) -> Self {
        let none = ChangeSet {
            mode: ChangeMode::Full(FullReason::NoReference),
            changed: Arc::new(Vec::new()),
        };
        Self::new(none, beam_dir, cwd)
    }

    /// `git.changes(glob)`: in incremental mode, the changed paths
    /// matching `glob` that still exist on disk; in full mode, the glob's
    /// base directory. Either way relative to the beam's `cwd`, `/`
    /// separated, sorted and without duplicates.
    ///
    /// `glob` is anchored to the Beamfile directory exactly as `inputs`
    /// are (see [`anchor_pattern`]) and matched with `globset`'s default
    /// dialect, as the cache and `--affected` match `inputs`, so the list
    /// agrees with what made the beam run. A glob that does not compile
    /// (impossible for one the load validated) gives full mode rather
    /// than a silently empty list.
    ///
    /// Fails with the offending path when one it would list is not valid
    /// UTF-8: a command is text, and a mangled name would hand the tool a
    /// file that does not exist.
    pub(crate) fn list(&self, glob: &str) -> Result<Vec<String>, PathBuf> {
        if self.mode != ChangeMode::Incremental {
            return Ok(vec![self.base_directory(glob)?]);
        }
        let (anchor, rest) = anchor_pattern(&self.beam_dir, glob);
        let anchor = lexically_normal(&anchor);
        let Ok(compiled) = globset::Glob::new(&rest) else {
            return Ok(vec![self.base_directory(glob)?]);
        };
        let matcher = compiled.compile_matcher();
        let cwd = lexical_components(&self.cwd);
        let mut paths = self
            .changed
            .iter()
            .filter(|path| {
                path.strip_prefix(&anchor).is_ok_and(|relative| {
                    matcher.is_match(relative.to_string_lossy().replace('\\', "/"))
                })
            })
            .filter(|path| path.exists())
            .map(|path| relative_text(&cwd, path))
            .collect::<Result<Vec<String>, PathBuf>>()?;
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// The deepest directory every match of `glob` lies under, relative to
    /// the beam's `cwd`, `.` when it is the `cwd` itself.
    fn base_directory(&self, glob: &str) -> Result<String, PathBuf> {
        let dir = pattern_dir(&self.beam_dir, glob);
        let relative = relative_text(&lexical_components(&self.cwd), &dir)?;
        Ok(if relative.is_empty() {
            ".".to_string()
        } else {
            relative
        })
    }
}

/// `path` relative to `cwd` (see [`relative_name`]), or `path` itself as
/// the error when it is not valid UTF-8. Only `path`'s own components and
/// `..` reach the result, so checking `path` is enough.
fn relative_text(cwd: &[std::path::Component<'_>], path: &Path) -> Result<String, PathBuf> {
    if path.to_str().is_none() {
        return Err(path.to_path_buf());
    }
    Ok(relative_name(cwd, path))
}

/// Compiles a `.dirs()` glob, `*` kept within one path segment (see
/// [`compile_segment_glob`]), so `src/*` names the directories directly
/// under `src`.
pub(crate) fn compile_dirs_glob(glob: &str, span: Span) -> Result<globset::GlobMatcher, CoreError> {
    compile_segment_glob(glob).map_err(|error| {
        CoreError::new(
            format!("invalid `dirs` glob `{glob}`: {}", error.kind()),
            span,
        )
    })
}

/// `.dirs(glob)`: each element becomes the shortest prefix of its path,
/// cut at a `/` boundary, that matches `glob`, or stays as it is when none
/// does. Then duplicates go, and so does an element lying under another
/// kept element, so a tool never processes a directory twice. Sorted.
pub(crate) fn dirs(items: &[String], matcher: &globset::GlobMatcher) -> Vec<String> {
    let mut reduced: Vec<String> = items
        .iter()
        .map(|item| {
            prefixes(item)
                .find(|prefix| matcher.is_match(prefix))
                .unwrap_or(item)
                .to_string()
        })
        .collect();
    reduced.sort();
    reduced.dedup();
    let kept: HashSet<&str> = reduced.iter().map(String::as_str).collect();
    reduced
        .iter()
        .filter(|item| !ancestors(item).any(|ancestor| kept.contains(ancestor)))
        .cloned()
        .collect()
}

/// The directories `path` lies under, as a list element names them: its
/// proper prefixes, and `.` (the `cwd`) unless `path` climbs out of it.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let inside_cwd = !matches!(path, "." | "..") && !path.starts_with("../");
    inside_cwd
        .then_some(".")
        .into_iter()
        .chain(prefixes(path).filter(move |prefix| *prefix != path))
}

/// `path`'s prefixes cut at each `/`, shortest first, `path` itself last.
fn prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(|(index, _)| &path[..index])
        .filter(|prefix| !prefix.is_empty())
        .chain(std::iter::once(path))
}

/// A list as it appears in a command: each element quoted for `quoting`,
/// joined by single spaces; nothing at all for an empty list.
pub(crate) fn render_list(items: &[String], quoting: Quoting) -> String {
    items
        .iter()
        .map(|item| quote(item, quoting))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One element, quoted so the shell reads it back as exactly one word.
/// An element made only of characters no shell treats specially is left
/// bare, so logs stay readable.
///
/// PowerShell additionally treats `,` (array) and `@` (splatting)
/// specially in an argument, so they are quoted there; its paths use the
/// native `\` separator. Its single-quoted strings end at any of the
/// single quote characters its grammar lists, typographic ones included,
/// so each of them is doubled, not only `'`.
///
/// An element starting with `-` is written `./-x.php` (`.\-x.php` for
/// PowerShell): names come from whatever a branch commits, and a bare
/// `--autoload-file=evil.php` would reach the tool as one of its options.
fn quote(item: &str, quoting: Quoting) -> String {
    let prefixed;
    let item = if item.starts_with('-') {
        prefixed = format!("./{item}");
        prefixed.as_str()
    } else {
        item
    };
    match quoting {
        Quoting::Posix => {
            if is_bare(item, "_./:@%+=,-") {
                item.to_string()
            } else {
                format!("'{}'", item.replace('\'', r"'\''"))
            }
        }
        Quoting::PowerShell => {
            let native = item.replace('/', "\\");
            if is_bare(item, "_./:%+=-") {
                native
            } else {
                let mut quoted = String::with_capacity(native.len() + 2);
                quoted.push('\'');
                for character in native.chars() {
                    if POWERSHELL_SINGLE_QUOTES.contains(&character) {
                        quoted.push(character);
                    }
                    quoted.push(character);
                }
                quoted.push('\'');
                quoted
            }
        }
    }
}

/// The characters PowerShell reads as a single quote: `'` and the
/// typographic `‘ ’ ‚ ‛` (U+2018 to U+201B).
const POWERSHELL_SINGLE_QUOTES: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];

/// Whether `item` is non-empty and made only of ASCII letters and digits,
/// non-ASCII letters, and the characters in `safe`.
fn is_bare(item: &str, safe: &str) -> bool {
    !item.is_empty()
        && item.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || safe.contains(character)
                || (!character.is_ascii() && character.is_alphabetic())
        })
}
