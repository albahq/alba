//! [`alba_core::git`] against a real temporary repository: every function
//! shells out to `git`, so nothing short of a repository can test them.

use std::path::Path;
use std::process::Command;

use alba_core::git::{
    KNOWN_HOOKS, changed_files, head, hooks_path, known_hook, repository_root, set_hooks_path,
    unset_hooks_path,
};

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository with one commit holding `src/lib.rs` and `README.md`.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "fn a() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "# x").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

#[test]
fn head_reports_branch_sha_and_a_clean_tree() {
    let dir = repository();
    let head = head(dir.path()).unwrap();
    assert_eq!(head.branch, "main");
    assert_eq!(head.sha.len(), 40);
    assert!(head.sha.starts_with(&head.short_sha));
    assert!(head.short_sha.len() >= 7 && head.short_sha.len() < 40);
    assert!(!head.dirty);
}

#[test]
fn an_untracked_file_makes_the_tree_dirty() {
    let dir = repository();
    std::fs::write(dir.path().join("new.txt"), "x").unwrap();
    assert!(head(dir.path()).unwrap().dirty);
}

#[test]
fn a_detached_head_reports_head_as_the_branch() {
    let dir = repository();
    git(dir.path(), &["checkout", "-q", "--detach"]);
    assert_eq!(head(dir.path()).unwrap().branch, "HEAD");
}

#[test]
fn changed_files_unions_the_diff_and_untracked_files() {
    let dir = repository();
    std::fs::write(dir.path().join("src/lib.rs"), "fn b() {}").unwrap();
    std::fs::remove_file(dir.path().join("README.md")).unwrap();
    std::fs::write(dir.path().join("src/new.rs"), "").unwrap();
    std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.path().join("ignored.txt"), "").unwrap();

    let changed = changed_files(dir.path(), "HEAD").unwrap();
    let names: Vec<String> = changed
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(
        names,
        [".gitignore", "README.md", "src/lib.rs", "src/new.rs"]
    );
}

#[test]
fn changed_files_are_relative_to_the_directory_asked_from() {
    let dir = repository();
    std::fs::write(dir.path().join("src/lib.rs"), "fn b() {}").unwrap();
    std::fs::write(dir.path().join("README.md"), "changed").unwrap();
    let changed = changed_files(&dir.path().join("src"), "HEAD").unwrap();
    assert_eq!(changed, [Path::new("lib.rs")]);
}

#[test]
fn changed_files_against_a_branch_sees_commits_since_it() {
    let dir = repository();
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    std::fs::write(dir.path().join("src/lib.rs"), "fn b() {}").unwrap();
    git(dir.path(), &["commit", "-q", "-am", "change"]);
    let changed = changed_files(dir.path(), "main").unwrap();
    assert_eq!(changed, [Path::new("src/lib.rs")]);
    assert!(changed_files(dir.path(), "HEAD").unwrap().is_empty());
}

#[test]
fn an_unknown_reference_is_a_git_error() {
    let dir = repository();
    let err = changed_files(dir.path(), "no-such-ref").unwrap_err();
    assert!(err.to_string().contains("no-such-ref"), "{err}");
}

#[test]
fn outside_a_repository_is_a_git_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(head(dir.path()).is_err());
    assert!(repository_root(dir.path()).is_err());
}

#[test]
fn repository_root_is_the_top_level_from_a_subdirectory() {
    let dir = repository();
    let root = repository_root(&dir.path().join("src")).unwrap();
    assert_eq!(root, dir.path().canonicalize().unwrap());
}

#[test]
fn hooks_path_round_trips_through_the_local_configuration() {
    let dir = repository();
    assert_eq!(hooks_path(dir.path()).unwrap(), None);
    set_hooks_path(dir.path(), ".alba/hooks").unwrap();
    assert_eq!(
        hooks_path(dir.path()).unwrap().as_deref(),
        Some(".alba/hooks")
    );
    unset_hooks_path(dir.path()).unwrap();
    assert_eq!(hooks_path(dir.path()).unwrap(), None);
}

#[test]
fn the_known_hooks_carry_git_s_argument_counts() {
    assert_eq!(known_hook("pre-commit").unwrap().arity, 0);
    assert_eq!(known_hook("commit-msg").unwrap().arity, 1);
    assert_eq!(known_hook("pre-push").unwrap().arity, 2);
    assert_eq!(known_hook("post-checkout").unwrap().arity, 3);
    assert!(known_hook("pre-comit").is_none());
    assert!(KNOWN_HOOKS.len() >= 15);
}
