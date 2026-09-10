//! End-to-end tests for the `alba` binary's non-running surface: `alba
//! check`, bare `alba` beam listing (with and without a `default`), and
//! `--file`/missing-Beamfile handling.

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

/// A beam with no parameters using the default (embedded-shell) executor
/// has its `run` template rendered and parsed statically: unsupported
/// syntax must surface at `check` time rather than waiting for `alba run`
/// to discover it. `alba check`'s own diagnostic goes to stderr, matching
/// every other Beamfile diagnostic this binary renders.
#[test]
fn check_rejects_invalid_embedded_shell_syntax() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam bad { run \"echo 'unclosed\" }",
    )
    .unwrap();
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("beam `bad`"))
        .stderr(predicates::str::contains("unclosed single quote"));
}

/// Four beams whose `run` command must never be validated statically,
/// each for a different reason:
///
/// - `deploy`'s template is unknowable until its argument arrives (its
///   unclosed quote here would only show up once `deploy` actually runs)
///   — and it references its own parameter, so a render attempt against
///   `beam.scope` alone would fail on its own, skipping it even without
///   an explicit parameter check.
/// - `legacy` opts out to the host shell, which does not speak this
///   grammar at all.
/// - `untouched` is the case that would slip past a render-failure
///   coincidence: it takes a parameter but never references it, so
///   rendering its template against `beam.scope` alone *succeeds* — an
///   implementation that skipped parameterized beams only because their
///   template failed to render (rather than checking `params.is_empty()`
///   directly) would validate this one anyway and reject its unclosed
///   quote, which must not happen.
/// - `ship` is the same argument applied to the docker executor: no
///   parameters, no reference to anything unrendered, so its template
///   renders cleanly too. Only the executor check keeps its unclosed
///   quote from ever reaching `alba_shell::parse` — a refactor that
///   rewrote the guard as a `match` naming `Shell` and `SystemShell` and
///   forgot `Docker` would start validating this beam against the
///   embedded shell's grammar, and this is the only thing in the suite
///   that would notice.
///
/// All four must be skipped, so `check` still exits 0 and still prints
/// its usual success line for all four beams.
#[test]
fn check_skips_parameterized_and_system_shell_beams() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam deploy(target) { run \"echo {target} 'x\" }\n\
         beam legacy { executor system_shell run \"if [ 1 ]; then echo y; fi\" }\n\
         beam untouched(unused) { run \"echo 'unclosed\" }\n\
         beam ship { executor docker { image \"x\" } run \"echo 'unclosed\" }\n",
    )
    .unwrap();
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .success()
        .stdout(predicates::str::contains("4 beams"));
}

/// A beam with no parameters and multiple valid embedded-shell commands
/// (including a pipeline) is validated and passes: `check` must not reject
/// the syntax the embedded shell actually supports.
#[test]
fn check_accepts_valid_embedded_commands() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam ok { run [\"echo one\", \"cat a.txt | cat\"] }",
    )
    .unwrap();
    alba()
        .current_dir(&dir)
        .arg("check")
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{2713} Beamfile: 1 beam"));
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

// A bare `alba` in a project that *does* declare a `default` belongs to
// `run` now that the placeholder it used to print is gone: see
// `cli_run.rs`'s `bare_alba_runs_the_default_beam`, which asserts the
// declared beam's command actually ran rather than that its name appeared.

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

/// `alba cache clean` removes the store `alba run` created, and cleaning
/// an already-clean project is still a success rather than an error.
#[test]
fn cache_clean_removes_the_store_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam gen { inputs [\"data.txt\"] run \"echo generated\" }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("data.txt"), "v1").unwrap();

    alba()
        .current_dir(&dir)
        .args(["run", "gen"])
        .assert()
        .success();
    assert!(dir.path().join(".alba/cache").is_dir());

    let clean = || {
        alba()
            .current_dir(&dir)
            .args(["cache", "clean"])
            .assert()
            .success()
    };
    clean();
    assert!(!dir.path().join(".alba/cache").exists());
    clean(); // nothing left to remove is still a success
}

/// A Beamfile that does not even parse must not block cleaning: the
/// command only needs the file's location, not its content.
#[test]
fn cache_clean_works_with_a_broken_beamfile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), "beam { this is not valid").unwrap();

    alba()
        .current_dir(&dir)
        .args(["cache", "clean"])
        .assert()
        .success();
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

/// Regression test: bare `alba`'s beam listing used to print through
/// `println!`, which panics as soon as its reader closes the pipe — the
/// exact thing `alba | head -1` does after `head` has its one line. Enough
/// beams are declared here that the listing's total output is well past
/// any realistic OS pipe buffer, so the child is still blocked writing
/// when this test drops its read end below, forcing at least one write to
/// actually fail rather than merely hoping a race lines up.
#[test]
fn bare_alba_survives_its_reader_closing_the_pipe_early() {
    let dir = tempfile::tempdir().unwrap();
    let beamfile: String = (0..20_000)
        .map(|i| {
            format!(
                "beam b{i} {{ description \"a fairly long description, number {i}, so the total listing is large\" run \"echo ok\" }}\n"
            )
        })
        .collect();
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_alba"))
        .current_dir(&dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Read a little, exactly like `head` would, then drop the pipe out
    // from under the still-writing child.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 16];
    std::io::Read::read_exact(&mut stdout, &mut buf).unwrap();
    drop(stdout);

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("panicked"),
        "listing must not panic when its reader closes the pipe early; stderr: {stderr}"
    );
    assert!(
        output.status.success(),
        "listing must still finish and exit 0 even though its reader went away; status: {:?}",
        output.status
    );
}
