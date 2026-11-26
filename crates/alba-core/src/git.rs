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

/// Spawns `git <args>` in `cwd` and returns its raw output. The only
/// concern here is whether the process could run at all: a missing `git`
/// binary is the fixed [`GitError`] every caller reports the same way.
/// Interpreting the exit status (success, or a status worth treating as
/// "no answer" rather than failure, as [`hooks_path`] does for `1`) is
/// left to the caller.
fn spawn(cwd: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitError("git not found on PATH".to_string())
            } else {
                GitError(format!("cannot run git: {error}"))
            }
        })
}

/// Runs `git <args>` in `cwd` and returns its raw stdout. A non-zero exit
/// is a [`GitError`] carrying the command and stderr.
fn git_output(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = spawn(cwd, args)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(GitError(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(output.stdout)
}

/// Runs `git <args>` in `cwd` and returns its stdout as text, trimmed of
/// the trailing newline.
fn git(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let stdout = git_output(cwd, args)?;
    let stdout = String::from_utf8_lossy(&stdout);
    Ok(stdout.trim_end_matches(['\n', '\r']).to_string())
}

/// Splits `-z`-terminated `git` output (paths separated by `NUL`, not
/// newline) into its entries. `-z` also turns off `core.quotepath`, so a
/// non-ASCII or special-character filename comes back verbatim instead of
/// C-quoted (`"caf\303\251.txt"`).
fn split_nul_terminated(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// The repository's top-level directory, canonical.
pub fn repository_root(cwd: &Path) -> Result<PathBuf, GitError> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    PathBuf::from(root)
        .canonicalize()
        .map_err(|error| GitError(format!("cannot canonicalize repository root: {error}")))
}

/// The full SHA `HEAD` points at, without the rest of [`head`]: no
/// working-tree scan.
pub fn head_sha(cwd: &Path) -> Result<String, GitError> {
    git(cwd, &["rev-parse", "HEAD"])
}

pub fn head(cwd: &Path) -> Result<GitHead, GitError> {
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = head_sha(cwd)?;
    let short_sha = git(cwd, &["rev-parse", "--short", "HEAD"])?;
    let status = git(cwd, &["status", "--porcelain", "--untracked-files=all"])?;
    Ok(GitHead {
        branch,
        sha,
        short_sha,
        dirty: !status.is_empty(),
    })
}

/// Refuses a `reference` git would read as one of its own options
/// (`--output=<file>` writes a file) instead of as a revision.
fn refuse_option_like(reference: &str) -> Result<(), GitError> {
    if reference.starts_with('-') {
        return Err(GitError(format!(
            "`{reference}` is not a git reference: it would be read as an option"
        )));
    }
    Ok(())
}

/// The full SHA of the best common ancestor of `reference` and `HEAD`:
/// where the current branch forked from `reference`.
///
/// Computed with `git merge-base` rather than `git diff --merge-base`,
/// which needs git 2.30. When there is none (unrelated histories, or a
/// shallow clone that did not fetch far enough) the error says so and
/// names the reference.
pub fn merge_base(cwd: &Path, reference: &str) -> Result<String, GitError> {
    refuse_option_like(reference)?;
    let output = spawn(cwd, &["merge-base", reference, "HEAD"])?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    // Unrelated histories make git exit 1 without a word on stderr.
    let mut message = format!("no merge base between `{reference}` and HEAD");
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        message.push_str(&format!(": {}", stderr.trim()));
    }
    let shallow =
        git(cwd, &["rev-parse", "--is-shallow-repository"]).is_ok_and(|out| out == "true");
    if shallow {
        message.push_str("; the clone is shallow, fetch more history (git fetch --unshallow)");
    }
    Err(GitError(message))
}

/// Every path that differs between `reference` and the working tree
/// (committed since, staged, modified, deleted, renamed) plus every
/// untracked file `.gitignore` does not exclude, anywhere in the
/// repository holding `cwd`; absolute, sorted, without duplicates.
///
/// The whole repository, not just `cwd`: a beam's `inputs` and imports may
/// climb out of its Beamfile's directory (`../libs/**`), and a change
/// there must affect it as surely as one beside it. Each path is built
/// from `cwd` and put in [`crate::lexically_normal`] form, the way an
/// `inputs` pattern's `..` is read, so the two compare equal without
/// touching the disk: a path under `cwd` is `cwd` joined with what lies
/// below it, and one outside climbs from `cwd` by as many `..` as git says
/// separate it from the top level.
///
/// "Since `reference`" means since the branch forked from it: the diff
/// starts at [`merge_base`], so commits that landed on `reference` after
/// the fork are not counted as the branch's changes.
///
/// A `reference` starting with `-` is refused: git would read it as one of
/// its own options (`--output=<file>` writes a file), not as a revision.
pub fn changed_files(cwd: &Path, reference: &str) -> Result<Vec<PathBuf>, GitError> {
    let base = merge_base(cwd, reference)?;
    changed_files_since(cwd, &base)
}

/// [`changed_files`] from `commit` itself rather than from its merge base
/// with `HEAD`: for a caller that already computed the merge base and
/// must not ask git for it twice. `commit` is refused when it starts with
/// `-`, as a reference is.
pub fn changed_files_since(cwd: &Path, commit: &str) -> Result<Vec<PathBuf>, GitError> {
    refuse_option_like(commit)?;
    let prefix = git_output(cwd, &["rev-parse", "--show-prefix"])?;
    let prefix = String::from_utf8_lossy(&prefix)
        .trim_end_matches(['\n', '\r'])
        .to_string();
    let climb = "../".repeat(prefix.split('/').filter(|part| !part.is_empty()).count());

    // Both list paths relative to the top level: `diff` without
    // `--relative` always does, `ls-files` once told `--full-name` and
    // the `:/` pathspec (the top level, wherever `cwd` is).
    let diff = git_output(cwd, &["diff", "--name-only", "-z", commit, "--"])?;
    let untracked = git_output(
        cwd,
        &[
            "ls-files",
            "-z",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            ":/",
        ],
    )?;
    let mut paths: Vec<PathBuf> = split_nul_terminated(&diff)
        .into_iter()
        .chain(split_nul_terminated(&untracked))
        .map(|path| {
            let joined = match path.strip_prefix(&prefix) {
                Ok(below) => cwd.join(below),
                Err(_) => cwd.join(&climb).join(path),
            };
            crate::lexically_normal(&joined)
        })
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// The repository-local `core.hooksPath`, if set.
pub fn hooks_path(cwd: &Path) -> Result<Option<String>, GitError> {
    // `git config --get` exits 1 when the key is unset: not an error here.
    let args = ["config", "--local", "--get", "core.hooksPath"];
    let output = spawn(cwd, &args)?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None),
        _ => Err(GitError(format!(
            "git {} failed: {}",
            args.join(" "),
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
