//! End-to-end tests for the external plugin executor, run through the
//! reference `alba-executor-example` plugin (`crates/alba-executor-example`)
//! rather than a scripted fake: this is the proof that a beam wired to
//! `executor example` really goes through a spawned process talking the
//! wire protocol, not just through `PluginExecutor`'s own unit tests.
//!
//! Every test here builds its own `Beamfile` in a fresh temporary
//! directory (`project`, mirroring `cli_run.rs`), and the two tests that
//! actually need the plugin resolved augment the child process's `PATH`
//! (`plugin_path`) rather than the test process's own — mutating
//! `std::env` here would race every other test running in this same
//! binary in parallel.

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

/// A temporary directory holding `beamfile` as its `Beamfile`.
fn project(beamfile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
    dir
}

/// `PATH` with the example plugin's directory prepended, so `alba`
/// resolves `alba-executor-example` the way a real user install would —
/// by finding it on the `PATH`, not by any special-casing of this test.
///
/// `cargo_bin` resolves a path already built into the workspace's target
/// directory (it does not build anything itself), which is why the full
/// gate matters here: `cargo test --workspace` builds every workspace
/// binary, `alba-executor-example` included, so by the time this runs the
/// file is already on disk for `cargo_bin` to find.
fn plugin_path() -> std::ffi::OsString {
    let example = assert_cmd::cargo::cargo_bin("alba-executor-example");
    let dir = example
        .parent()
        .expect("a binary path always has a parent directory")
        .to_path_buf();

    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![dir];
    dirs.extend(std::env::split_paths(&existing));
    std::env::join_paths(dirs).expect("the augmented PATH must join into one OsString")
}

/// A beam wired to `executor example` runs its command through the
/// spawned plugin process end to end: `alba` resolves the binary, speaks
/// the handshake, sends `execute`, and relays the plugin's `output` line
/// back into the beam's own stdout.
#[test]
fn a_plugin_beam_runs_end_to_end() {
    let dir =
        project("beam hello { executor example run \"echo carried by the example plugin\" }\n");

    alba()
        .current_dir(&dir)
        .env("PATH", plugin_path())
        .args(["run", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains("carried by the example plugin"));
}

/// The plugin's `exit` message with a non-zero code is an ordinary beam
/// failure, reported through the same summary as a failing shell command
/// would be — the plugin boundary is invisible to the run's outcome.
#[test]
fn a_plugin_beams_failure_is_an_ordinary_beam_failure() {
    let dir = project("beam boom { executor example run \"fail 3\" }\n");

    alba()
        .current_dir(&dir)
        .env("PATH", plugin_path())
        .args(["run", "boom"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("1 failed"));
}

/// Without the augmented `PATH`, `alba-executor-nosuchthing` is not on it,
/// so resolving the beam's executor fails before the run ever starts: a
/// clean plan-time error (exit 2), not a spawn failure surfacing mid-run.
/// This pins the CLI-visible form of the message the previous task's
/// plan-time resolution already produces.
#[test]
fn a_missing_plugin_is_a_clean_plan_time_error() {
    let dir = project("beam ghost { executor nosuchthing run \"echo nope\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "ghost"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "neither a built-in executor nor `alba-executor-nosuchthing`",
        ));
}
