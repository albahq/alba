//! End-to-end tests for `alba affected <ref>` and `alba run --affected
//! <ref>`, against a real temporary repository. Commands are `echo`-only.

use std::path::Path;
use std::process::Command;

use predicates::prelude::PredicateBooleanExt;

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const BEAMFILE: &str = "\
beam codegen { inputs [\"schema/**\"] run \"echo ran-codegen\" }
beam build { needs [codegen] inputs [\"src/**\"] run \"echo ran-build\" }
beam test { needs [build] run \"echo ran-test\" }
beam docs { inputs [\"docs/**\"] run \"echo ran-docs\" }
beam deploy(target) { inputs [\"deploy/**\"] run \"echo ran-deploy {target}\" }
";

fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("Beamfile"), BEAMFILE).unwrap();
    for file in ["schema/a", "src/a", "docs/a", "deploy/a"] {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "v1").unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

#[test]
fn affected_lists_the_beams_a_change_touches() {
    let dir = repository();
    std::fs::write(dir.path().join("schema/a"), "v2").unwrap();
    alba()
        .current_dir(&dir)
        .args(["affected", "HEAD"])
        .assert()
        .success()
        .stdout("codegen\nbuild\ntest\n");
}

#[test]
fn affected_marks_parameterized_beams_and_speaks_json() {
    let dir = repository();
    std::fs::write(dir.path().join("deploy/a"), "v2").unwrap();
    alba()
        .current_dir(&dir)
        .args(["affected", "HEAD"])
        .assert()
        .success()
        .stdout("deploy (takes parameters)\n");
    alba()
        .current_dir(&dir)
        .args(["affected", "HEAD", "--log-format", "json"])
        .assert()
        .success()
        .stdout("{\"beams\":[\"deploy\"]}\n");
}

#[test]
fn affected_with_nothing_changed_lists_nothing_and_exits_0() {
    let dir = repository();
    alba()
        .current_dir(&dir)
        .args(["affected", "HEAD"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn run_affected_without_a_beam_runs_every_affected_beam() {
    let dir = repository();
    std::fs::write(dir.path().join("src/a"), "v2").unwrap();
    std::fs::write(dir.path().join("docs/a"), "v2").unwrap();
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD"])
        .assert()
        .success()
        .stdout(
            predicates::str::contains("ran-build")
                .and(predicates::str::contains("ran-test"))
                .and(predicates::str::contains("ran-docs"))
                // `codegen` runs as `build`'s need, not as a target.
                .and(predicates::str::contains("ran-codegen")),
        );
}

#[test]
fn run_affected_within_a_beam_runs_it_only_when_affected() {
    let dir = repository();
    std::fs::write(dir.path().join("docs/a"), "v2").unwrap();
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD", "test"])
        .assert()
        .success()
        .stdout("")
        .stderr(predicates::str::contains("nothing affected by HEAD"));
    std::fs::write(dir.path().join("src/a"), "v2").unwrap();
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD", "test"])
        .assert()
        .success()
        .stdout(
            predicates::str::contains("ran-test").and(predicates::str::contains("ran-docs").not()),
        );
}

#[test]
fn run_affected_json_carries_the_reference_and_targets() {
    let dir = repository();
    std::fs::write(dir.path().join("docs/a"), "v2").unwrap();
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD", "--log-format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(first["event"], "run_started");
    assert_eq!(first["affected_by"], "HEAD");
    assert_eq!(first["targets"], serde_json::json!(["docs"]));
}

/// The JSON counterpart of `run_affected_within_a_beam_runs_it_only_when_affected`'s
/// nothing-affected case: the stream still opens with `run_started` even
/// though no beam is affected, mirroring the watch session's equivalent
/// assertion in `cli_watch.rs`.
#[test]
fn run_affected_with_nothing_affected_still_opens_the_json_stream() {
    let dir = repository();
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD", "--log-format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let first: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!(first["event"], "run_started");
    assert_eq!(first["affected_by"], "HEAD");
    assert_eq!(first["targets"], serde_json::json!([]));
}

#[test]
fn run_affected_against_an_unknown_reference_exits_2() {
    let dir = repository();
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "no-such-ref"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no-such-ref"));
}

#[test]
fn run_affected_outside_a_repository_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), BEAMFILE).unwrap();
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("git"));
}
