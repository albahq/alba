//! Filesystem resolution for `inputs`/`outputs` declarations: which files
//! a beam's glob patterns actually name on disk.
//!
//! The two functions deliberately answer different questions against
//! different views of the disk. [`expand_globs`] resolves `inputs` for
//! fingerprinting and is `.gitignore`-aware: hashing `src/**/*` must not
//! aspirate build artifacts a `.gitignore` already declares uninteresting.
//! [`outputs_satisfied`] checks `outputs` for presence and queries the raw
//! disk: outputs typically live in exactly those ignored directories
//! (`target/`, `dist/`), so filtering them would make their patterns never
//! match.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

/// The files matching any of `patterns`, resolved from `base`, as
/// `(relative path with '/' separators, absolute path)` pairs sorted by
/// relative path — sorted so a fingerprint built from the list is
/// deterministic across platforms and directory iteration orders.
///
/// A pattern may climb out of `base` through leading `..` components
/// (`../../Dockerfile`): each pattern is walked from its own
/// [`anchor_pattern`], and every file is named by its shortest path from
/// `base`, `../` included, so a file reached by two spellings of the same
/// path is listed once.
///
/// `.gitignore` files are honored even outside a git repository
/// (`require_git(false)`), so behaviour does not silently change when a
/// project is unpacked from a tarball; only the project's own ignore files
/// apply, not the machine's global excludes, so inputs remain reproducible
/// regardless of whose machine resolves them. Hidden files are included —
/// `inputs [".env"]` must work — but `.git/` and `.alba/` are never
/// walked: the first is noise, and hashing the cache's own directory
/// would invalidate every beam on every run. Symbolic links are followed:
/// a linked source file (or a linked source directory, as a vendored or
/// workspace-shared tree often is) is a real input, and dropping it for
/// not being a regular file would silently hash nothing.
///
/// Never fails. `inputs`/`outputs` patterns are validated when the
/// Beamfile loads, so a pattern that does not compile here can only come
/// from a [`crate::Project`] assembled some other way; it is skipped on
/// its own, leaving its siblings' matches intact.
pub fn expand_globs(base: &Path, patterns: &[String]) -> Vec<(String, PathBuf)> {
    let mut anchors: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for pattern in patterns {
        let (anchor, rest) = anchor_pattern(base, pattern);
        match anchors.iter_mut().find(|(dir, _)| *dir == anchor) {
            Some((_, group)) => group.push(rest),
            None => anchors.push((anchor, vec![rest])),
        }
    }

    let base_components = lexical_components(base);
    let mut files: Vec<(String, PathBuf)> = anchors
        .iter()
        .flat_map(|(anchor, group)| walk(anchor, group))
        .map(|absolute| (relative_name(&base_components, &absolute), absolute))
        .collect();
    files.sort();
    files.dedup_by(|later, earlier| later.0 == earlier.0);
    files
}

/// Where `pattern`'s matches live: `base` with the pattern's leading `.`
/// and `..` components applied, and the rest of the pattern, relative to
/// that directory. `../../Dockerfile` declared in `/repo/contexts/shared`
/// anchors at `/repo` as `Dockerfile`.
///
/// Lexical on purpose: `..` names the parent of the directory as the
/// Beamfile spells it, the way its author reads it, not the parent of
/// wherever a symbolic link on the way leads. Every consumer that matches
/// a path against `inputs` (the cache, affected runs, watch mode) anchors
/// through here, so they cannot disagree about what a pattern names.
pub fn anchor_pattern(base: &Path, pattern: &str) -> (PathBuf, String) {
    let mut anchor = base.to_path_buf();
    let mut rest = pattern;
    loop {
        if let Some(tail) = rest.strip_prefix("./") {
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("../") {
            if anchor.file_name().is_some() {
                anchor.pop();
            } else if !anchor.has_root() {
                // A relative base already at, or above, its own start.
                anchor.push("..");
            }
            rest = tail;
        } else {
            return (anchor, rest.to_string());
        }
    }
}

/// The absolute paths of the files under `anchor` matching `patterns`,
/// which are relative to it.
fn walk(anchor: &Path, patterns: &[String]) -> Vec<PathBuf> {
    let matcher = Matcher::compile(patterns);
    if matcher.is_empty() {
        return Vec::new();
    }

    // Owned copies: the walker's filter must outlive this call's borrows.
    let (root, roots) = (anchor.to_path_buf(), matcher.roots.clone());
    let walker = ignore::WalkBuilder::new(anchor)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .follow_links(true)
        .filter_entry(move |entry| {
            entry.file_name() != OsStr::new(".git")
                && entry.file_name() != OsStr::new(".alba")
                && worth_visiting(&root, entry.path(), &roots)
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(anchor) else {
            continue;
        };
        let unified = relative.to_string_lossy().replace('\\', "/");
        if matcher.set.is_match(&unified) {
            files.push(entry.into_path());
        }
    }
    files
}

/// `path`'s components with `.` dropped and `..` folded into the component
/// before it, so two spellings of one path compare equal.
fn lexical_components(path: &Path) -> Vec<Component<'_>> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if matches!(components.last(), Some(Component::Normal(_))) => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    components
}

/// The shortest '/'-separated path from the base (given as its
/// [`lexical_components`]) to `path`, climbing with `..` when `path` lies
/// outside it.
fn relative_name(base: &[Component<'_>], path: &Path) -> String {
    let path = lexical_components(path);
    let shared = base
        .iter()
        .zip(&path)
        .take_while(|(left, right)| left == right)
        .count();
    let climb = std::iter::repeat_n("..".to_string(), base.len() - shared);
    let descend = path[shared..]
        .iter()
        .map(|component| component.as_os_str().to_string_lossy().into_owned());
    climb.chain(descend).collect::<Vec<_>>().join("/")
}

/// Whether every `outputs` pattern is satisfied on disk: a literal path
/// must exist, a glob pattern must match at least one entry. Raw disk, no
/// `.gitignore` filtering — see the module doc comment.
pub fn outputs_satisfied(base: &Path, patterns: &[String]) -> bool {
    patterns.iter().all(|pattern| {
        if pattern.contains(['*', '?', '[']) {
            glob::glob(&base.join(pattern).to_string_lossy())
                .map(|mut matches| matches.any(|entry| entry.is_ok()))
                .unwrap_or(false)
        } else {
            base.join(pattern).exists()
        }
    })
}

/// A beam's patterns, compiled: what a path is matched against, and which
/// directories can possibly hold a match.
struct Matcher {
    set: globset::GlobSet,
    /// Per pattern, its leading run of literal path components — `crates`
    /// for `crates/**/*.rs`, empty for `**/*.rs`. Nothing outside one of
    /// these can match, so nothing outside one of them is walked.
    roots: Vec<Vec<String>>,
}

impl Matcher {
    /// Compiles `patterns` one at a time, keeping the ones that compile.
    /// A single bad pattern used to abandon the whole set, turning every
    /// declared input into "nothing matched" — which the cache reads as a
    /// beam whose inputs never change, and therefore never reruns.
    fn compile(patterns: &[String]) -> Self {
        let mut builder = globset::GlobSetBuilder::new();
        let mut roots = Vec::new();
        for pattern in patterns {
            if let Ok(glob) = globset::Glob::new(pattern) {
                builder.add(glob);
                roots.push(literal_prefix(pattern));
            }
        }
        Self {
            // `build` only fails on a glob that already compiled, but the
            // cache degrades rather than panics on anything unexpected.
            set: builder
                .build()
                .unwrap_or_else(|_| globset::GlobSet::empty()),
            roots,
        }
    }

    fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

/// Whether `path` is, or could contain, a match — the walk's pruning rule.
///
/// The `GlobSet` alone only filters entries the walk has already produced,
/// so `crates/**/*.rs` visited the whole project, `target/` included, once
/// per beam declaring it. A path matching no pattern's literal prefix can
/// hold no match, so descent stops there. Deliberately conservative: a
/// pattern with no literal prefix (`**/*.rs`) still walks everything, and
/// which files end up matched is unchanged.
fn worth_visiting(base: &Path, path: &Path, roots: &[Vec<String>]) -> bool {
    let Ok(relative) = path.strip_prefix(base) else {
        return true;
    };
    let components: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    // `zip` stops at the shorter side, so this holds when either is a
    // prefix of the other: a directory on the way down to a root, and
    // anything underneath one.
    roots.iter().any(|root| {
        root.iter()
            .zip(&components)
            .all(|(expected, actual)| expected == actual)
    })
}

/// The leading path components of `pattern` that are plain text, stopping
/// at the first one holding a glob metacharacter.
fn literal_prefix(pattern: &str) -> Vec<String> {
    const META: [char; 7] = ['*', '?', '[', ']', '{', '}', '\\'];
    pattern
        .split('/')
        .take_while(|component| !component.is_empty() && !component.contains(META))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_dot_components_move_the_anchor_and_leave_the_rest() {
        let anchor = |base: &str, pattern: &str| {
            let (dir, rest) = anchor_pattern(Path::new(base), pattern);
            (dir.to_string_lossy().replace('\\', "/"), rest)
        };

        assert_eq!(
            anchor("/repo/contexts/shared", "../../Dockerfile"),
            ("/repo".to_string(), "Dockerfile".to_string())
        );
        assert_eq!(
            anchor("/repo", "./src/**/*.rs"),
            ("/repo".to_string(), "src/**/*.rs".to_string())
        );
        assert_eq!(
            anchor("/repo", "src/**/*.rs"),
            ("/repo".to_string(), "src/**/*.rs".to_string())
        );
        // A filesystem root has no parent: climbing past it stays there.
        assert_eq!(anchor("/", "../x"), ("/".to_string(), "x".to_string()));
        // A relative base keeps climbing above its own start.
        assert_eq!(anchor(".", "../x"), ("./..".to_string(), "x".to_string()));
    }

    #[test]
    fn a_name_climbs_out_of_the_base_only_as_far_as_needed() {
        let base = lexical_components(Path::new("/repo/contexts/shared"));
        let name = |path: &str| relative_name(&base, Path::new(path));

        assert_eq!(name("/repo/contexts/shared/src/a.php"), "src/a.php");
        assert_eq!(name("/repo/Dockerfile"), "../../Dockerfile");
        assert_eq!(
            name("/repo/contexts/shared/../../Dockerfile"),
            "../../Dockerfile"
        );
        assert_eq!(name("/repo/contexts/other/b.php"), "../other/b.php");
    }

    #[test]
    fn a_literal_prefix_stops_at_the_first_metacharacter() {
        assert_eq!(literal_prefix("crates/**/*.rs"), vec!["crates"]);
        assert_eq!(
            literal_prefix("crates/alba-core/src/*.rs"),
            vec!["crates", "alba-core", "src"]
        );
        assert_eq!(literal_prefix("Cargo.toml"), vec!["Cargo.toml"]);
        assert!(literal_prefix("**/*.rs").is_empty());
        assert!(literal_prefix("/absolute/x.rs").is_empty());
    }

    /// A pattern with no literal prefix must not prune anything: the
    /// pruning rule may only remove paths that cannot match.
    #[test]
    fn a_pattern_without_a_literal_prefix_visits_everything() {
        let base = Path::new("/project");
        let roots = vec![Vec::new()];
        assert!(worth_visiting(base, Path::new("/project/target"), &roots));
    }

    #[test]
    fn only_the_way_down_to_a_root_and_its_contents_are_visited() {
        let base = Path::new("/project");
        let roots = vec![vec!["crates".to_string(), "alba-core".to_string()]];

        assert!(worth_visiting(base, Path::new("/project/crates"), &roots));
        assert!(worth_visiting(
            base,
            Path::new("/project/crates/alba-core/src/files.rs"),
            &roots
        ));
        assert!(!worth_visiting(base, Path::new("/project/target"), &roots));
        assert!(!worth_visiting(
            base,
            Path::new("/project/crates/alba-cli"),
            &roots
        ));
        // A sibling whose name merely starts with a root's name is not
        // under it: components are compared whole, never as substrings.
        assert!(!worth_visiting(
            base,
            Path::new("/project/crates-extra"),
            &roots
        ));
    }
}
