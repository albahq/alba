//! End-to-end tests for the git hooks: install, a real `git commit`
//! running the hook, a commit blocked by a failing beam, uninstall, and
//! `alba check`'s reporting.

use std::path::Path;
use std::process::{Command, Output};

use predicates::prelude::PredicateBooleanExt;

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

/// Runs git with the `alba` binary under test on the PATH, so an installed
/// hook script finds it.
fn git(dir: &Path, args: &[&str]) -> Output {
    let bin_dir = assert_cmd::cargo::cargo_bin("alba")
        .parent()
        .unwrap()
        .to_path_buf();
    let mut paths = vec![bin_dir];
    paths.extend(
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    let path = std::env::join_paths(paths).unwrap();
    Command::new("git")
        .current_dir(dir)
        .env("PATH", path)
        .args(args)
        .output()
        .unwrap()
}

fn git_ok(dir: &Path, args: &[&str]) {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(beamfile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git_ok(dir.path(), &["init", "-q", "-b", "main"]);
    git_ok(
        dir.path(),
        &["config", "--local", "user.email", "alba@example.com"],
    );
    git_ok(dir.path(), &["config", "--local", "user.name", "Alba"]);
    git_ok(
        dir.path(),
        &["config", "--local", "commit.gpgsign", "false"],
    );
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
    std::fs::write(dir.path().join(".gitignore"), ".alba/\n").unwrap();
    git_ok(dir.path(), &["add", "-A"]);
    git_ok(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

const PASSING: &str = "hook pre-commit { beam check }\nbeam check { run \"echo hook-ran\" }\n";
const FAILING: &str = "hook pre-commit { beam check }\nbeam check { run \"exit 3\" }\n";
const COMMIT_MSG: &str =
    "hook commit-msg { beam lint }\nbeam lint(path) { run \"echo msg-at {path}\" }\n";

#[test]
fn install_points_hooks_path_at_alba_and_writes_every_known_hook() {
    let dir = repository(PASSING);
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
    let config = git(
        dir.path(),
        &["config", "--local", "--get", "core.hooksPath"],
    );
    assert_eq!(
        String::from_utf8_lossy(&config.stdout).trim(),
        ".alba/hooks"
    );
    assert!(dir.path().join(".alba/hooks/pre-commit").is_file());
    assert!(dir.path().join(".alba/hooks/pre-push").is_file());
    assert!(dir.path().join(".alba/hooks/commit-msg").is_file());
    // Idempotent.
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
}

#[test]
fn a_commit_runs_the_declared_hook() {
    let dir = repository(PASSING);
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
    std::fs::write(dir.path().join("file.txt"), "x").unwrap();
    git_ok(dir.path(), &["add", "file.txt"]);
    let output = git(dir.path(), &["commit", "-q", "-m", "change"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stdout.contains("hook-ran") || stderr.contains("hook-ran"),
        "stdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn a_failing_hook_blocks_the_commit() {
    let dir = repository(FAILING);
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
    std::fs::write(dir.path().join("file.txt"), "x").unwrap();
    git_ok(dir.path(), &["add", "file.txt"]);
    let output = git(dir.path(), &["commit", "-q", "-m", "change"]);
    assert!(!output.status.success());
    let log = git(dir.path(), &["log", "--oneline"]);
    assert_eq!(
        String::from_utf8_lossy(&log.stdout).lines().count(),
        1,
        "the commit must not have landed"
    );
}

#[test]
fn git_s_arguments_become_the_target_s_parameters() {
    let dir = repository(COMMIT_MSG);
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
    std::fs::write(dir.path().join("file.txt"), "x").unwrap();
    git_ok(dir.path(), &["add", "file.txt"]);
    let output = git(dir.path(), &["commit", "-q", "-m", "change"]);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{combined}");
    assert!(
        combined.contains("msg-at ") && combined.contains("COMMIT_EDITMSG"),
        "{combined}"
    );
}

#[test]
fn an_undeclared_hook_is_silent_and_succeeds() {
    let dir = repository(PASSING);
    alba()
        .current_dir(&dir)
        .args(["hook", "pre-push", "origin", "url"])
        .assert()
        .success()
        .stdout("")
        .stderr("");
    // No Beamfile at all: same answer.
    let empty = tempfile::tempdir().unwrap();
    alba()
        .current_dir(&empty)
        .args(["hook", "pre-commit"])
        .assert()
        .success()
        .stdout("")
        .stderr("");
}

/// The silent-zero exit is only for the default `./Beamfile` lookup an
/// installed script relies on. An explicit `--file` naming a path that
/// does not exist is a typo the caller must hear about, not a second way
/// to spell "no Beamfile here".
#[test]
fn an_explicit_missing_file_fails_the_hook_loudly() {
    let empty = tempfile::tempdir().unwrap();
    alba()
        .current_dir(&empty)
        .args(["--file", "no-such-Beamfile", "hook", "pre-commit"])
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicates::str::contains("no-such-Beamfile"));
}

#[test]
fn a_broken_beamfile_fails_the_hook_loudly() {
    let dir = repository("beam x { descriptoin \"t\" }");
    alba()
        .current_dir(&dir)
        .args(["hook", "pre-commit"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("did you mean"));
}

#[test]
fn install_refuses_a_foreign_hooks_path_and_uninstall_restores() {
    let dir = repository(PASSING);
    git_ok(
        dir.path(),
        &["config", "--local", "core.hooksPath", ".husky"],
    );
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "core.hooksPath already points to `.husky`",
        ));
    alba()
        .current_dir(&dir)
        .args(["hooks", "uninstall"])
        .assert()
        .code(2);
    git_ok(
        dir.path(),
        &["config", "--local", "--unset", "core.hooksPath"],
    );

    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
    alba()
        .current_dir(&dir)
        .args(["hooks", "uninstall"])
        .assert()
        .success();
    let config = git(
        dir.path(),
        &["config", "--local", "--get", "core.hooksPath"],
    );
    assert_eq!(
        config.status.code(),
        Some(1),
        "core.hooksPath must be unset"
    );
    assert!(!dir.path().join(".alba/hooks").exists());
}

#[test]
fn check_counts_hooks_and_warns_until_they_are_installed() {
    let dir = repository(PASSING);
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .success()
        .stdout(predicates::str::contains("1 beam, 1 hook\n"))
        .stderr(predicates::str::contains(
            "1 hook declared, run 'alba hooks install'",
        ));
    alba()
        .current_dir(&dir)
        .args(["hooks", "install"])
        .assert()
        .success();
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .success()
        .stderr(predicates::str::contains("hooks install").not());
}
