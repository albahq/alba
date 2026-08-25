//! The one place Alba talks to git: a thin wrapper over the `git` CLI.
//!
//! Shelling out rather than linking a library is the same call the docker
//! executor made: no native dependency, and git is present on any machine
//! that has hooks to install. Every function takes the directory to run
//! from; paths come back relative to it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A git invocation that could not answer: git missing from the PATH, no
/// repository, an unknown reference. Carries git's own words.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GitError(pub String);

/// What `HEAD` looks like right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHead {
    /// The branch name, or `HEAD` when detached.
    pub branch: String,
    pub sha: String,
    pub short_sha: String,
    /// Whether the index or the working tree differs from `HEAD`,
    /// untracked files included.
    pub dirty: bool,
}

/// A hook git knows, with the number of arguments it passes. Server-side
/// hooks are left out: a developer's clone never fires them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownHook {
    pub name: &'static str,
    /// The guaranteed argument count. `prepare-commit-msg` and
    /// `pre-rebase` receive more in some situations; the minimum is what
    /// a beam may rely on.
    pub arity: usize,
}

pub const KNOWN_HOOKS: &[KnownHook] = &[
    KnownHook {
        name: "applypatch-msg",
        arity: 1,
    },
    KnownHook {
        name: "pre-applypatch",
        arity: 0,
    },
    KnownHook {
        name: "post-applypatch",
        arity: 0,
    },
    KnownHook {
        name: "pre-commit",
        arity: 0,
    },
    KnownHook {
        name: "pre-merge-commit",
        arity: 0,
    },
    KnownHook {
        name: "prepare-commit-msg",
        arity: 1,
    },
    KnownHook {
        name: "commit-msg",
        arity: 1,
    },
    KnownHook {
        name: "post-commit",
        arity: 0,
    },
    KnownHook {
        name: "pre-rebase",
        arity: 1,
    },
    KnownHook {
        name: "post-checkout",
        arity: 3,
    },
    KnownHook {
        name: "post-merge",
        arity: 1,
    },
    KnownHook {
        name: "pre-push",
        arity: 2,
    },
    KnownHook {
        name: "pre-auto-gc",
        arity: 0,
    },
    KnownHook {
        name: "post-rewrite",
        arity: 1,
    },
    KnownHook {
        name: "sendemail-validate",
        arity: 1,
    },
    KnownHook {
        name: "post-index-change",
        arity: 2,
    },
];

pub fn known_hook(name: &str) -> Option<&'static KnownHook> {
    KNOWN_HOOKS.iter().find(|hook| hook.name == name)
}

/// Runs `git <args>` in `cwd` and returns its stdout, trimmed of the
/// trailing newline. A non-zero exit is a [`GitError`] carrying stderr.
fn git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitError("git not found on PATH".to_string())
            } else {
                GitError(format!("cannot run git: {error}"))
            }
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim_end_matches(['\n', '\r']).to_string())
}

/// The repository's top-level directory, canonical.
pub fn repository_root(cwd: &Path) -> Result<PathBuf, GitError> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    PathBuf::from(root)
        .canonicalize()
        .map_err(|error| GitError(format!("cannot canonicalize repository root: {error}")))
}

pub fn head(cwd: &Path) -> Result<GitHead, GitError> {
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = git(cwd, &["rev-parse", "HEAD"])?;
    let short_sha = git(cwd, &["rev-parse", "--short", "HEAD"])?;
    let status = git(cwd, &["status", "--porcelain", "--untracked-files=all"])?;
    Ok(GitHead {
        branch,
        sha,
        short_sha,
        dirty: !status.is_empty(),
    })
}

/// Every path that differs between `reference` and the working tree
/// (committed since, staged, modified, deleted, renamed) plus every
/// untracked file `.gitignore` does not exclude; relative to `cwd`,
/// sorted, without duplicates. Paths outside `cwd` are not reported.
pub fn changed_files(cwd: &Path, reference: &str) -> Result<Vec<PathBuf>, GitError> {
    let diff = git(cwd, &["diff", "--name-only", "--relative", reference])?;
    let untracked = git(cwd, &["ls-files", "--others", "--exclude-standard"])?;
    let mut paths: Vec<PathBuf> = diff
        .lines()
        .chain(untracked.lines())
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The repository-local `core.hooksPath`, if set.
pub fn hooks_path(cwd: &Path) -> Result<Option<String>, GitError> {
    // `git config --get` exits 1 when the key is unset: not an error here.
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["config", "--local", "--get", "core.hooksPath"])
        .output()
        .map_err(|error| GitError(format!("cannot run git: {error}")))?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => Err(GitError(format!(
            "git config failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

pub fn set_hooks_path(cwd: &Path, value: &str) -> Result<(), GitError> {
    git(cwd, &["config", "--local", "core.hooksPath", value]).map(|_| ())
}

pub fn unset_hooks_path(cwd: &Path) -> Result<(), GitError> {
    git(cwd, &["config", "--local", "--unset", "core.hooksPath"]).map(|_| ())
}
