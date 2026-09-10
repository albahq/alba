//! End-to-end tests for the interface's activation rule.
//!
//! `assert_cmd` captures stdout through a pipe, so every run here is off a
//! terminal — which is exactly the half of the rule that matters most to
//! guarantee: whatever the interface does on a real terminal, piping `alba`
//! must never meet an escape code. That half, and the refusal `--ui` earns
//! down a pipe, are what these can prove; the interface's own side is
//! covered under a pseudo-terminal.

use predicates::prelude::PredicateBooleanExt;

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

/// A temporary directory holding `beamfile` as its `Beamfile`.
fn project(beamfile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
    dir
}

/// The escape sequences an interface leaking into a pipe would be caught
/// by: entering the alternate screen, and clearing the screen.
const ALTERNATE_SCREEN: &str = "\u{1b}[?1049";
const CLEAR_SCREEN: &str = "\u{1b}[2J";

fn assert_no_escape_codes(stdout: &str, context: &str) {
    for sequence in [ALTERNATE_SCREEN, CLEAR_SCREEN] {
        assert!(
            !stdout.contains(sequence),
            "{context} must not carry a terminal escape sequence, got:\n{stdout:?}"
        );
    }
}

/// Off a terminal, the default stays headless: the beam's output arrives as
/// plain text, with nothing in it a pager or a log file would choke on.
#[test]
fn a_pipe_gets_headless_output_by_default() {
    let dir = project("beam ok { run \"echo hello-from-the-beam\" }\n");

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "ok"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains("hello-from-the-beam"),
        "the beam's output is missing from:\n{stdout}"
    );
    assert_no_escape_codes(&stdout, "a piped run");
}

/// `--no-ui` off a terminal changes nothing: it is the state the pipe was
/// already in, and it must not become an error or a second code path.
#[test]
fn asking_for_no_ui_down_a_pipe_is_an_ordinary_run() {
    let dir = project("beam ok { run \"echo hello-from-the-beam\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "ok", "--no-ui"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hello-from-the-beam"));
}

/// `--ui` cannot draw down a pipe, so it is refused before anything runs:
/// exit 2, and a reason. Filling the reader's stream with escape codes, or
/// quietly ignoring the flag, are both worse answers.
#[test]
fn forcing_the_ui_without_a_terminal_exits_2() {
    let dir = project("beam ok { run \"echo hello-from-the-beam\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "ok", "--ui"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("--ui needs a terminal"))
        // Refused *before* the run: the beam must not have executed.
        .stdout(predicates::str::contains("hello-from-the-beam").not());
}

/// The two flags are opposites, and clap rejects the pair rather than
/// letting the activation rule quietly pick a winner.
#[test]
fn ui_and_no_ui_together_are_rejected() {
    let dir = project("beam ok { run \"echo hello-from-the-beam\" }\n");

    // This asserts on clap's own conflict text as plain, uncoloured
    // output, so the child's colour has to be pinned off regardless of
    // what the parent test process's own environment carries: a `test`
    // beam run under the interface sets `FORCE_COLOR`/`CLICOLOR_FORCE` on
    // this process (see `commands/run.rs`), and clap honours those over a
    // piped, non-terminal stderr unless `NO_COLOR` says otherwise.
    alba()
        .current_dir(&dir)
        .env("NO_COLOR", "1")
        .args(["run", "ok", "--ui", "--no-ui"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("cannot be used with"));
}

/// A machine-readable stream is a machine-readable stream: every line still
/// parses as JSON, with no escape code anywhere in it.
#[test]
fn json_mode_never_opens_the_interface() {
    let dir = project("beam ok { run \"echo hello-from-the-beam\" }\n");

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "ok", "--log-format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    for line in stdout.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"));
    }
    assert_no_escape_codes(&stdout, "the JSON stream");
}

/// An explicit `--output` is a request for a text layout, and it is
/// honoured as one: the grouped renderer's beam header, not an interface.
#[test]
fn an_explicit_output_style_stays_text() {
    let dir = project("beam ok { run \"echo hello-from-the-beam\" }\n");

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "ok", "--output", "grouped"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains("hello-from-the-beam"),
        "the beam's output is missing from:\n{stdout}"
    );
    assert_no_escape_codes(&stdout, "an explicit text layout");
}
