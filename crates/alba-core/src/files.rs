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
use std::path::{Path, PathBuf};

/// The files under `base` matching any of `patterns`, as
/// `(relative path with '/' separators, absolute path)` pairs sorted by
/// relative path — sorted so a fingerprint built from the list is
/// deterministic across platforms and directory iteration orders.
///
/// `.gitignore` files are honored even outside a git repository
/// (`require_git(false)`), so behaviour does not silently change when a
/// project is unpacked from a tarball; only the project's own ignore files
/// apply, not the machine's global excludes, so inputs remain reproducible
/// regardless of whose machine resolves them. Hidden files are included —
/// `inputs [".env"]` must work — but `.git/` and `.alba/` are never
/// walked: the first is noise, and hashing the cache's own directory
/// would invalidate every beam on every run.
///
/// Never fails: patterns were validated when the Beamfile loaded, and a
/// pattern this function still cannot compile yields an empty list, which
/// the cache treats as "nothing matched".
pub fn expand_globs(base: &Path, patterns: &[String]) -> Vec<(String, PathBuf)> {
    let Some(set) = build_globset(patterns) else {
        return Vec::new();
    };

    let walker = ignore::WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .filter_entry(|entry| {
            entry.file_name() != OsStr::new(".git") && entry.file_name() != OsStr::new(".alba")
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(base) else {
            continue;
        };
        let unified = relative.to_string_lossy().replace('\\', "/");
        if set.is_match(&unified) {
            files.push((unified, entry.into_path()));
        }
    }
    files.sort();
    files
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

fn build_globset(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(globset::Glob::new(pattern).ok()?);
    }
    builder.build().ok()
}
