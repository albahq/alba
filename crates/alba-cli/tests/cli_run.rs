//! End-to-end tests for `alba run`: dependency ordering, failure and
//! cancellation semantics, the `--log-format json` stream, parameter
//! interpolation, and the two ways a run can fail before it starts (a
//! docker beam, an unknown target).
//!
//! Every Beamfile here runs commands that behave the same on macOS, Linux,
//! and Windows: `echo <word>` and `exit <n>` are all that is needed, and
//! both work identically in POSIX `sh` and in `powershell`. The two
//! interrupt tests are the exception and are `#[cfg(unix)]`, since sending
//! a signal to another process has no windows equivalent here.
//!
//! Colour is off in all of these: `assert_cmd` captures stdout through a
//! pipe, which `crate::color_enabled()` reports as "not a terminal", so
//! output can be compared as plain text.

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

/// `b` needs `a`, so `a`'s output must appear before `b`'s whatever the
/// scheduler's parallelism allows.
#[test]
fn runs_dependency_chain_in_order() {
    let dir = project(
        "beam a { run \"echo alpha\" }\n\
         beam b { needs [a] run \"echo bravo\" }\n",
    );

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "b"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    let alpha = stdout
        .find("alpha")
        .unwrap_or_else(|| panic!("`a`'s output is missing from:\n{stdout}"));
    let bravo = stdout
        .find("bravo")
        .unwrap_or_else(|| panic!("`b`'s output is missing from:\n{stdout}"));
    assert!(
        alpha < bravo,
        "`a` must be reported before its dependent `b`, got:\n{stdout}"
    );
}

/// A failing beam exits the process 1, and its dependent never runs — so
/// its output never appears. The stderr summary counts the failure.
#[test]
fn beam_failure_exits_1_and_cancels_dependents() {
    let dir = project(
        "beam boom { run \"exit 7\" }\n\
         beam after { needs [boom] run \"echo reached\" }\n",
    );

    alba()
        .current_dir(&dir)
        .args(["run", "after"])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("reached").not())
        .stderr(predicates::str::contains("1 failed"));
}

/// `allow_failure true` turns a non-zero exit into a reported-but-tolerated
/// outcome: the process still exits 0, and the beam is labelled as such.
#[test]
fn allow_failure_exits_0() {
    let dir = project("beam lint { allow_failure true run \"exit 1\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "lint"])
        .assert()
        .success()
        .stdout(predicates::str::contains("failed (allowed)"));
}

/// `--log-format json` makes stdout a machine-readable stream: every line
/// is one JSON object, and the four event kinds are all present.
#[test]
fn json_log_format_emits_one_json_object_per_line() {
    let dir = project("beam a { run \"echo hi\" }\n");

    let assert = alba()
        .current_dir(&dir)
        .args(["run", "a", "--log-format", "json"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    let mut kinds = Vec::new();
    for line in stdout.lines() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("stdout line is not JSON ({error}): {line:?}"));
        let kind = value["event"]
            .as_str()
            .unwrap_or_else(|| panic!("JSON line has no `event` field: {line:?}"));
        kinds.push(kind.to_string());
    }

    for expected in [
        "beam_started",
        "beam_output",
        "beam_finished",
        "run_finished",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "missing `{expected}` among {kinds:?}"
        );
    }
}

/// A beam's declared parameter is bound from the run's positional
/// arguments and interpolated into its `run` template.
#[test]
fn beam_params_are_interpolated() {
    let dir = project("beam greet(name) { run \"echo hello {name}\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "greet", "world"])
        .assert()
        .success()
        .stdout(predicates::str::contains("hello world"));
}

/// `executor docker` parses, but no docker executor exists yet: the run is
/// rejected before anything executes, as an Alba error (exit 2).
#[test]
fn docker_executor_is_rejected() {
    let dir = project("beam ship { executor docker { image \"x\" } run \"echo hi\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "ship"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("not yet supported"));
}

/// A misspelled target is an Alba error (exit 2) with the loader's
/// "did you mean...?" suggestion rendered as a diagnostic.
#[test]
fn unknown_beam_suggests_closest() {
    let dir = project("beam build { run \"echo ok\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "biuld"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("did you mean `build`?"));
}

/// Bare `alba` in a project that declares a `default` runs that beam
/// rather than listing beams.
#[test]
fn bare_alba_runs_the_default_beam() {
    let dir = project("default build\nbeam build { run \"echo compiled\" }\n");

    alba()
        .current_dir(&dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("compiled"));
}

/// `--jobs 0` is rejected by argument parsing rather than silently
/// meaning something: a run with no slots is not a run.
#[test]
fn jobs_zero_is_rejected() {
    let dir = project("beam a { run \"echo ok\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "a", "--jobs", "0"])
        .assert()
        .code(2);
}

/// Ctrl-C handling. Alba puts every command in its own process group, so a
/// terminal Ctrl-C no longer reaches those children — Alba is the only
/// thing that can stop them, which is what these two tests pin down.
#[cfg(unix)]
mod interrupts {
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use super::project;

    /// Long enough for the binary to start, load the Beamfile, and get a
    /// command actually running before the signal lands.
    const STARTUP: Duration = Duration::from_millis(2000);

    fn spawn(dir: &tempfile::TempDir, beam: &str) -> Child {
        Command::new(assert_cmd::cargo::cargo_bin("alba"))
            .current_dir(dir.path())
            .args(["run", beam])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn interrupt(child: &Child) {
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
    }

    /// The first signal cancels the run: Alba announces it, terminates the
    /// command it started, and exits on its own rather than being killed.
    #[test]
    fn first_interrupt_cancels_the_run() {
        let dir = project("beam slow { run \"sleep 30\" }\n");
        let child = spawn(&dir, "slow");
        std::thread::sleep(STARTUP);

        interrupt(&child);
        let output = child.wait_with_output().unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("cancelling"),
            "the first signal must announce the cancellation, got:\n{stderr}"
        );
    }

    /// The second signal aborts immediately, without waiting for the
    /// executor's grace period. The beam ignores `SIGTERM` and loops for a
    /// bounded time, so the run is provably still cancelling (inside the
    /// executor's 5 s grace) when the second signal lands.
    #[test]
    fn second_interrupt_aborts_the_process() {
        let dir = project(
            "beam stubborn { run \"trap '' TERM; i=0; while [ $i -lt 40 ]; do sleep 0.25; i=$((i+1)); done\" }\n",
        );
        let child = spawn(&dir, "stubborn");
        std::thread::sleep(STARTUP);

        interrupt(&child);
        std::thread::sleep(Duration::from_millis(500));
        interrupt(&child);
        let output = child.wait_with_output().unwrap();

        assert_eq!(
            output.status.code(),
            Some(130),
            "the second signal must abort with the conventional SIGINT code, stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
