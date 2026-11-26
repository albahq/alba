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
    // The machine's global excludes would otherwise apply: one that
    // ignores `Beamfile` leaves it uncommitted and every assertion about it
    // answering for that machine instead of for the code under test.
    git(
        dir.path(),
        &["config", "core.excludesFile", ".git/no-global-excludes"],
    );
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

// ---- the reference a `git` block declares ----------------------------

/// A project declaring `main` as its base, committed on `main` and checked
/// out on a fresh `feature` branch: the Beamfile itself is part of the
/// merge base, so it never forces full mode. Nothing ignores `.alba/`:
/// the cache Alba writes inside the tree is no change of the branch.
const BASED: &str = "\
git { base 'main' }
beam lint { inputs [\"src/**\"] run \"echo files: {git.changes('src/**')}\" }
beam docs { inputs [\"docs/**\"] run \"echo docs: {git.changes('docs/**')}\" }
beam deploy { run \"echo ran-deploy\" }
";

fn based_repository(beamfile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    git(
        dir.path(),
        &["config", "core.excludesFile", ".git/no-global-excludes"],
    );
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
    for file in ["src/a", "src/b", "docs/a"] {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "v1").unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    dir
}

/// Commits `files` (each rewritten) on the current branch.
fn commit_changes(dir: &Path, files: &[&str]) {
    for file in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "v2").unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "change"]);
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

fn json_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn alba_run_without_flags_lists_only_changed_files() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src/a\n"));
}

/// Changed files whose names hold a space, a quote or a non-ASCII
/// character reach the command as one argument each, through both git
/// listings (committed and untracked): `cat` reads every one whole, where
/// split words would name `src/a` and a `b.php` that does not exist, and
/// a name git quoted would match no file on disk and drop out.
#[test]
fn posix_quoting_end_to_end() {
    let dir = based_repository(
        "git { base 'main' }\n\
         beam lint { inputs [\"src/**\"] run \"cat {git.changes('src/**')}\" }\n",
    );
    std::fs::write(dir.path().join("src/a b.php"), "read-space\n").unwrap();
    std::fs::write(dir.path().join("src/café.php"), "read-accent\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "awkward names"]);
    // Untracked: listed by `ls-files`, not by the diff.
    std::fs::write(dir.path().join("src/it's.php"), "read-quote\n").unwrap();
    alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(
            predicates::str::contains("read-space")
                .and(predicates::str::contains("read-accent"))
                .and(predicates::str::contains("read-quote")),
        );
}

/// The same through `executor system_shell`, whose host shell (`sh -c`)
/// splits words as the embedded shell does: a changed file whose name
/// holds a space and a quote still reaches `cat` as one argument.
#[cfg(unix)]
#[test]
fn posix_quoting_through_the_system_shell() {
    let dir = based_repository(
        "git { base 'main' }\n\
         beam lint { executor system_shell inputs [\"src/**\"] run \"cat {git.changes('src/**')}\" }\n",
    );
    std::fs::write(dir.path().join("src/a b.php"), "read-space\n").unwrap();
    std::fs::write(dir.path().join("src/it's.php"), "read-quote\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "awkward names"]);
    alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(
            predicates::str::contains("read-space").and(predicates::str::contains("read-quote")),
        );
}

/// On Windows the system shell is PowerShell: the list is quoted its way,
/// with native separators, and `Get-Content` reads the name whole. One
/// file: `Get-Content` takes a single positional path.
#[cfg(windows)]
#[test]
fn powershell_quoting_through_the_system_shell() {
    let dir = based_repository(
        "git { base 'main' }\n\
         beam lint { executor system_shell inputs [\"src/**\"] run \"Get-Content {git.changes('src/**')}\" }\n",
    );
    std::fs::write(dir.path().join("src/it's a.php"), "read-awkward\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "awkward name"]);
    alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("read-awkward"));
}

#[test]
fn affected_without_a_value_uses_the_declared_base() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "--affected", "--log-format", "json"])
        .assert()
        .success();
    let events = json_lines(&stdout_of(&assert));
    assert_eq!(events[0]["event"], "run_started");
    assert_eq!(events[0]["affected_by"], "main");
    assert_eq!(events[0]["targets"], serde_json::json!(["lint"]));

    // The beam before the flag: `--affected lint` would read `lint` as the
    // reference.
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .args(["run", "lint", "--affected"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src/a\n"));
    alba()
        .current_dir(&dir)
        .args(["run", "docs", "--affected"])
        .assert()
        .success()
        .stdout("")
        .stderr(predicates::str::contains("nothing affected by main"));

    // An empty value means the same.
    alba()
        .current_dir(&dir)
        .args(["run", "--affected=", "docs"])
        .assert()
        .success()
        .stderr(predicates::str::contains("nothing affected by main"));
}

#[test]
fn affected_with_a_value_still_works() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "main", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src/a\n"));
}

/// Clap never reads a flag as `--affected`'s value.
#[test]
fn affected_without_a_value_leaves_the_next_flag_alone() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "--affected", "--no-ui", "--log-format", "json"])
        .assert()
        .success();
    let events = json_lines(&stdout_of(&assert));
    assert_eq!(events[0]["affected_by"], "main");
}

#[test]
fn affected_without_a_value_and_no_base_exits_2() {
    let dir = repository();
    alba()
        .current_dir(&dir)
        .args(["run", "--affected"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("git { base"));
}

#[test]
fn affected_without_a_value_and_an_unresolved_base_exits_2() {
    let dir = based_repository(&BASED.replace("'main'", "'origin/nope'"));
    alba()
        .current_dir(&dir)
        .args(["run", "--affected"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("origin/nope"));
}

#[test]
fn no_affected_runs_full() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .args(["run", "lint", "--no-affected"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src\n"));
}

#[test]
fn affected_and_no_affected_conflict() {
    let dir = based_repository(BASED);
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "main", "--no-affected"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with"));
}

/// The shallow clone of a CI checkout: the declared base is not there to
/// diff from, so the run warns once and stays whole rather than failing.
#[test]
fn an_unresolved_base_warns_and_runs_full() {
    let dir = based_repository(&BASED.replace("'main'", "'origin/nope'"));
    commit_changes(dir.path(), &["src/a"]);
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src\n"));
    let stderr = stderr_of(&assert);
    assert_eq!(
        stderr
            .matches("warning: cannot diff from `origin/nope`: ")
            .count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("; running in full mode"), "{stderr}");

    // The interleaved layout prints the same single warning.
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "lint", "--output", "interleaved", "--no-cache"])
        .assert()
        .success();
    let stderr = stderr_of(&assert);
    assert_eq!(
        stderr
            .matches("warning: cannot diff from `origin/nope`: ")
            .count(),
        1,
        "{stderr}"
    );
}

#[test]
fn an_unresolved_explicit_reference_exits_2() {
    let dir = based_repository(BASED);
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "--affected", "no-such-ref", "lint"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no-such-ref"));
    // Reported once, not once per place that asked git.
    let stderr = stderr_of(&assert);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

#[test]
fn affected_without_a_value_and_no_affected_conflict() {
    let dir = based_repository(BASED);
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "--no-affected"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("cannot be used with"));
}

/// A bare `alba` runs the declared `default` beam, diffing from the
/// declared base like `alba run`.
#[test]
fn a_bare_alba_runs_the_default_beam_incrementally() {
    let dir = based_repository(&format!("default lint\n{BASED}"));
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src/a\n"));
}

/// A `base` that reads git, outside a repository (a Docker build context
/// without `.git`): the project loads with no reference, and the run says
/// why once.
#[test]
fn a_git_block_outside_a_repository_warns_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        BASED.replace("'main'", "match git.branch { _ => 'main' }"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a"), "v1").unwrap();
    let assert = alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src\n"));
    let stderr = stderr_of(&assert);
    assert_eq!(stderr.matches("warning: ").count(), 1, "{stderr}");
    assert!(
        stderr.contains("; no reference declared for this run"),
        "{stderr}"
    );
}

#[test]
fn beams_without_inputs_still_run_with_a_base() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .args(["run", "deploy"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ran-deploy"));
}

#[test]
fn alba_affected_uses_the_declared_base() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["src/a"]);
    alba()
        .current_dir(&dir)
        .args(["affected"])
        .assert()
        .success()
        .stdout("lint\n");
}

#[test]
fn alba_affected_without_reference_or_base_exits_2() {
    let dir = repository();
    alba()
        .current_dir(&dir)
        .args(["affected"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("git { base"));
}

/// The JSON stream of a run diffing from the declared base: a beam with
/// nothing to process ends `unchanged`, one with changes starts in
/// incremental mode, and `run_started` names the reference and its merge
/// base.
#[test]
fn the_json_stream_reports_the_base_the_mode_and_unchanged_beams() {
    let dir = based_repository(
        "git { base 'main' }\n\
         beam lint { inputs [\"src/**\"] run \"echo files: {git.changes('src/**')}\" }\n\
         beam docs { inputs [\"docs/**\"] run \"echo docs: {git.changes('docs/**')}\" }\n\
         beam all { needs [lint, docs] run \"echo ran-all\" }\n",
    );
    commit_changes(dir.path(), &["docs/a"]);
    let merge_base = Command::new("git")
        .current_dir(&dir)
        .args(["merge-base", "main", "HEAD"])
        .output()
        .unwrap();
    let merge_base = String::from_utf8(merge_base.stdout).unwrap();

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "all", "--log-format", "json"])
        .assert()
        .success();
    let events = json_lines(&stdout_of(&assert));

    assert_eq!(
        events[0]["base"],
        serde_json::json!({ "reference": "main", "merge_base": merge_base.trim() })
    );
    let docs_started = events
        .iter()
        .find(|event| event["event"] == "beam_started" && event["beam"] == "docs")
        .unwrap();
    assert_eq!(docs_started["mode"], "incremental");
    assert!(docs_started.get("full_reason").is_none(), "{docs_started}");
    let lint_finished = events
        .iter()
        .find(|event| event["event"] == "beam_finished" && event["beam"] == "lint")
        .unwrap();
    assert_eq!(lint_finished["status"], "unchanged");
    let run_finished = events.last().unwrap();
    assert_eq!(run_finished["event"], "run_finished");
    assert_eq!(run_finished["unchanged"], serde_json::json!(["lint"]));
    assert_eq!(run_finished["exit_code"], 0);
}

#[test]
fn an_unchanged_beam_reads_unchanged_in_both_text_layouts() {
    let dir = based_repository(BASED);
    commit_changes(dir.path(), &["docs/a"]);
    alba()
        .current_dir(&dir)
        .args(["run", "lint", "--output", "grouped"])
        .assert()
        .success()
        .stdout(predicates::str::contains("── lint (○ unchanged) ──"));
    alba()
        .current_dir(&dir)
        .args(["run", "lint", "--output", "interleaved"])
        .assert()
        .success()
        .stdout(predicates::str::contains("○ unchanged"));
}

/// A repository on `main` declaring `main` as its base, with nothing
/// ignoring `.alba/`: the cache Alba writes beside the Beamfile is an
/// untracked directory git reports.
fn target_branch_repository(beamfile: &str, files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    git(
        dir.path(),
        &["config", "core.excludesFile", ".git/no-global-excludes"],
    );
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
    for file in files {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "v1").unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

/// Alba's own cache, untracked inside the repository, is not the
/// branch's work: after a first run on the target branch, a commit that
/// changes an input still finds the tree clean, so the next run is full
/// and runs the beam instead of ending it `unchanged`.
#[test]
fn the_cache_inside_the_repository_keeps_the_target_branch_full() {
    let dir = target_branch_repository(
        "git { base 'main' }\n\
         beam lint { inputs [\"src/**/*.php\"] run \"echo files: {git.changes('src/**/*.php')}\" }\n",
        &["src/a.php"],
    );
    alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("files: src\n"));
    assert!(dir.path().join(".alba/cache").is_dir());

    std::fs::write(dir.path().join("src/a.php"), "v2").unwrap();
    // Only the input: `git add -A` would commit the cache and clean the
    // tree by accident.
    git(dir.path(), &["add", "src/a.php"]);
    git(dir.path(), &["commit", "-q", "-m", "change"]);

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "lint", "--log-format", "json"])
        .assert()
        .success();
    let events = json_lines(&stdout_of(&assert));
    let finished = events
        .iter()
        .find(|event| event["event"] == "beam_finished" && event["beam"] == "lint")
        .unwrap();
    assert_eq!(finished["status"], "succeeded", "{finished}");
    let started = events
        .iter()
        .find(|event| event["event"] == "beam_started" && event["beam"] == "lint")
        .unwrap();
    assert_eq!(started["mode"], "full", "{started}");
    assert_eq!(started["full_reason"], "on_target_branch", "{started}");
}

/// The affected selection ignores Alba's own state the same way: a beam
/// reading every file of the project is not affected by the cache its
/// own run wrote.
#[test]
fn the_cache_inside_the_repository_affects_nothing() {
    let dir = target_branch_repository(
        "beam all { inputs [\"**\"] run \"echo ran-all\" }\n",
        &["src/a"],
    );
    alba()
        .current_dir(&dir)
        .args(["run", "all"])
        .assert()
        .success();
    assert!(dir.path().join(".alba/cache").is_dir());

    alba()
        .current_dir(&dir)
        .args(["affected", "HEAD"])
        .assert()
        .success()
        .stdout("");
    alba()
        .current_dir(&dir)
        .args(["run", "--affected", "HEAD"])
        .assert()
        .success()
        .stderr(predicates::str::contains("nothing affected by HEAD"));
}

/// A full mode names what forced it: the changed input outside every
/// `git.changes()` glob, relative to the project root, or the dependency
/// through which the beam is affected. An incremental beam carries
/// neither the reason nor its detail.
#[test]
fn beam_started_names_what_forced_full_mode() {
    let dir = based_repository(
        "git { base 'main' }\n\
         beam install { inputs [\"composer.lock\"] run \"echo installed\" }\n\
         beam phpcs { needs [install] inputs [\"src/**/*.php\"] run \"echo phpcs {git.changes('src/**/*.php')}\" }\n\
         beam lint { inputs [\"src/**/*.php\", \"config/phpcs.xml\"] run \"echo lint {git.changes('src/**/*.php')}\" }\n\
         beam docs { inputs [\"docs/**\"] run \"echo docs {git.changes('docs/**')}\" }\n\
         beam all { needs [phpcs, lint, docs] run \"echo ran-all\" }\n",
    );
    commit_changes(dir.path(), &["composer.lock", "config/phpcs.xml", "docs/a"]);

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "all", "--log-format", "json"])
        .assert()
        .success();
    let events = json_lines(&stdout_of(&assert));
    let started = |beam: &str| {
        events
            .iter()
            .find(|event| event["event"] == "beam_started" && event["beam"] == beam)
            .unwrap_or_else(|| panic!("`{beam}` starts"))
            .clone()
    };

    let lint = started("lint");
    assert_eq!(lint["full_reason"], "input_outside_changes", "{lint}");
    assert_eq!(lint["full_reason_detail"], "config/phpcs.xml", "{lint}");
    let phpcs = started("phpcs");
    assert_eq!(phpcs["full_reason"], "dependency_affected", "{phpcs}");
    assert_eq!(phpcs["full_reason_detail"], "install", "{phpcs}");
    let docs = started("docs");
    assert_eq!(docs["mode"], "incremental", "{docs}");
    assert!(docs.get("full_reason_detail").is_none(), "{docs}");
    // A reason that names nothing carries no detail.
    let install = started("install");
    assert!(install.get("full_reason_detail").is_none(), "{install}");
}
