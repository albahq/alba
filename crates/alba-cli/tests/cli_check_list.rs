//! End-to-end tests for the `alba` binary surface this task adds: `alba
//! check`, bare `alba` beam listing (with and without a `default`), and the
//! `--file`/missing-Beamfile handling every later task's tests build on.

use predicates::prelude::PredicateBooleanExt;

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

#[test]
fn check_reports_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam build { run \"echo ok\" }",
    )
    .unwrap();
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .success()
        .stdout(predicates::str::contains("1 beam"));
}

#[test]
fn check_renders_parse_error_with_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), "beam x { descriptoin \"t\" }").unwrap();
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("did you mean `description`?"));
}

/// Beamfile with two described beams and no `default` declaration: a bare
/// `alba` invocation must list both ids and both descriptions on stdout,
/// sorted by id (`build` before `test`), and exit 0.
#[test]
fn bare_alba_lists_beams_without_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam test { description \"Run the test suite\" run \"echo ok\" }\n\
         beam build { description \"Compile the workspace\" run \"echo ok\" }\n",
    )
    .unwrap();

    let output = alba().current_dir(&dir).assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    assert!(stdout.contains("build"));
    assert!(stdout.contains("Compile the workspace"));
    assert!(stdout.contains("test"));
    assert!(stdout.contains("Run the test suite"));
    // Sorted by id: `build` (which sorts before `test`) must appear first.
    assert!(stdout.find("build").unwrap() < stdout.find("test").unwrap());
}

#[test]
fn missing_beamfile_is_exit_2() {
    alba()
        .current_dir(tempfile::tempdir().unwrap().path())
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no Beamfile found"));
}

/// A beam declared without a `description` gets the dimmed placeholder —
/// asserted with color disabled (assert_cmd's captured stdout is not a
/// TTY), so the plain text must appear with no surrounding ANSI escapes.
#[test]
fn bare_alba_shows_placeholder_for_missing_description() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam build { run \"echo ok\" }",
    )
    .unwrap();

    alba()
        .current_dir(&dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("(no description)"))
        .stdout(predicates::str::contains("\u{1b}[").not());
}

/// A `default` beam is declared: a bare `alba` must not fall back to
/// listing beams. Until Task 12 wires a real `run`, the placeholder output
/// only needs to name the target — this asserts exactly that and nothing
/// about the placeholder's exact wording, so Task 12 can replace it freely.
#[test]
fn bare_alba_with_default_names_the_target_instead_of_listing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "default build\nbeam build { description \"Compile\" run \"echo ok\" }\n",
    )
    .unwrap();

    alba()
        .current_dir(&dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("build"));
}

#[test]
fn file_flag_points_at_a_beamfile_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Other.beam"),
        "beam build { run \"echo ok\" }",
    )
    .unwrap();

    alba()
        .current_dir(&dir)
        .args(["--file", "Other.beam", "check"])
        .assert()
        .success()
        .stdout(predicates::str::contains("1 beam"));
}

#[test]
fn file_flag_missing_target_is_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    alba()
        .current_dir(&dir)
        .args(["--file", "some/where/Other.beam", "check"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no Beamfile found"));
}
