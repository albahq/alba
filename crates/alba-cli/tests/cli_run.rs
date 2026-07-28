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
///
/// The interpolated value is quoted (`'{name}'`) so it reaches the shell
/// as a single token, per this file's `echo <word>` invariant (see the
/// module doc comment): an unquoted multi-word `echo hello {name}` would
/// still print "hello world" under POSIX `sh`, but under `powershell`,
/// `echo` (an alias for `Write-Output`) treats each bare word as a
/// separate pipeline object and prints one per line instead of joining
/// them with spaces.
#[test]
fn beam_params_are_interpolated() {
    let dir = project("beam greet(name) { run \"echo '{name}'\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "greet", "hello world"])
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
/// meaning something: a run with no slots is not a run. The message is
/// asserted, not only the code, since exit 2 alone would also be produced
/// by any other Alba error on the way there.
#[test]
fn jobs_zero_is_rejected() {
    let dir = project("beam a { run \"echo ok\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run", "a", "--jobs", "0"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "invalid value '0' for '--jobs <N>'",
        ));
}

/// The three specified output shapes, pinned as literal strings so a
/// refactor cannot quietly change any of them. Durations are deliberately
/// never asserted: they are the one part that legitimately varies.
mod layout {
    use super::{alba, project};

    /// `beam-id │ text`, one line per event, with the beam's output
    /// carried verbatim after the separator.
    #[test]
    fn interleaved_prefixes_every_line_with_its_beam() {
        let dir = project("beam a { run \"echo alpha\" }\n");

        let assert = alba()
            .current_dir(&dir)
            .args(["run", "a", "--output", "interleaved"])
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

        let lines: Vec<&str> = stdout.lines().collect();
        assert!(
            lines.contains(&"a \u{2502} started"),
            "missing the start line in:\n{stdout}"
        );
        assert!(
            lines.contains(&"a \u{2502} alpha"),
            "missing the prefixed output line in:\n{stdout}"
        );
        // The duration is the only part deliberately left unasserted.
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("a \u{2502} ok in ")),
            "missing the status line in:\n{stdout}"
        );
    }

    /// `── beam-id (1.2s, ok) ──`, with the beam's lines underneath it.
    #[test]
    fn grouped_frames_each_beam_with_a_header() {
        let dir = project("beam a { run \"echo alpha\" }\n");

        // `--output grouped` explicitly rather than relying on the piped
        // default, so this test still pins the shape if that default ever
        // changes.
        let assert = alba()
            .current_dir(&dir)
            .args(["run", "a", "--output", "grouped"])
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

        let lines: Vec<&str> = stdout.lines().collect();
        let header = lines
            .iter()
            .position(|line| line.starts_with("\u{2500}\u{2500} a ("))
            .unwrap_or_else(|| panic!("no group header in:\n{stdout}"));
        assert!(
            lines[header].ends_with(", ok) \u{2500}\u{2500}"),
            "header does not match `── a (<duration>, ok) ──`: {:?}",
            lines[header]
        );
        assert_eq!(
            lines.get(header + 1),
            Some(&"alpha"),
            "the beam's output must follow its header in:\n{stdout}"
        );
    }

    /// `✓ 1 succeeded · ✗ 1 failed · ⊘ 1 cancelled · 4.1s` on stderr. The
    /// chain `good → bad → blocked` makes all three buckets deterministic
    /// whatever `--jobs` resolves to: `good` must finish before `bad` can
    /// start, and `blocked` can never start at all.
    #[test]
    fn the_summary_reports_every_non_empty_bucket_on_stderr() {
        let dir = project(
            "beam good { run \"echo g\" }\n\
             beam bad { needs [good] run \"exit 3\" }\n\
             beam blocked { needs [bad] run \"echo never\" }\n",
        );

        let assert = alba()
            .current_dir(&dir)
            .args(["run", "blocked"])
            .assert()
            .code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();

        let summary = stderr.lines().next_back().unwrap_or_default();
        let expected =
            "\u{2713} 1 succeeded \u{b7} \u{2717} 1 failed \u{b7} \u{2298} 1 cancelled \u{b7} ";
        assert!(
            summary.starts_with(expected),
            "summary does not match `{expected}<duration>`: {summary:?}"
        );
        assert!(
            summary.ends_with('s'),
            "the summary must end with a duration: {summary:?}"
        );
    }
}

/// `alba run x | head -1` — the reader walks away mid-run. Under
/// `println!` this panicked the renderer, lost the summary, and handed
/// back exit 0 for a run that had failed. Every renderer now writes
/// through a `LineSink`, so a dead stdout is silence, not a crash.
///
/// The read end is closed before the run starts rather than after a line
/// or two: it makes the test deterministic (every single write fails, on
/// every platform) instead of racing the pipe's buffer.
#[test]
fn a_closed_stdout_neither_panics_nor_changes_the_exit_code() {
    // The third element says whether this mode reports a summary on
    // stderr: `json` deliberately does not, since its `run_finished` line
    // already carries every count — and that line went to the stdout
    // nobody is reading, which is the point of the exit-code assertion.
    for (mode, summarizes) in [
        (["--output", "interleaved"], true),
        (["--output", "grouped"], true),
        (["--log-format", "json"], false),
    ] {
        let dir = project("beam bad { run \"exit 7\" }\n");

        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("alba"))
            .current_dir(dir.path())
            .args(["run", "bad"])
            .args(mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        drop(child.stdout.take());

        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            !stderr.contains("panicked"),
            "{mode:?}: a closed stdout must not panic a renderer:\n{stderr}"
        );
        assert_eq!(
            output.status.code(),
            Some(1),
            "{mode:?}: the run's exit code must survive a closed stdout, stderr:\n{stderr}"
        );
        assert!(
            !summarizes || stderr.contains("1 failed"),
            "{mode:?}: the summary must still reach stderr:\n{stderr}"
        );
    }
}

/// Ctrl-C handling. Alba puts every command in its own process group, so a
/// terminal Ctrl-C no longer reaches those children — Alba is the only
/// thing that can stop them, which is what these two tests pin down.
#[cfg(unix)]
mod interrupts {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::project;

    /// How long to wait for the beam to report that it is running before
    /// giving up. Not a synchronisation delay — the test never waits this
    /// long on the happy path — just a bound so a broken binary fails the
    /// test instead of hanging the suite.
    const READY_TIMEOUT: Duration = Duration::from_secs(30);

    /// Spawns `alba run <beam>` and returns once the beam's own command
    /// has printed `ready` on stdout.
    ///
    /// This waits on a real event, not a duration. A fixed sleep before
    /// the first signal is not merely imprecise here, it can invert the
    /// test: a `SIGINT` that arrives before `watch_interrupts` has
    /// installed its handler is handled by the default disposition, which
    /// kills `alba` outright — no `cancelling...`, no exit code, and both
    /// tests below fail for a reason that has nothing to do with what they
    /// assert. Seeing the beam's own output proves the runtime is up, the
    /// engine is scheduling, the watcher task was spawned ahead of it, and
    /// a shell child is running and waiting to be killed.
    ///
    /// `--output interleaved` because the grouped default buffers a beam's
    /// output until it finishes, which is exactly never here.
    fn spawn_running(dir: &tempfile::TempDir, beam: &str) -> Child {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("alba"))
            .current_dir(dir.path())
            .args(["run", beam, "--output", "interleaved"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let stdout = child.stdout.take().expect("stdout was piped");
        let (ready, is_ready) = mpsc::channel();
        // Reads to EOF rather than stopping at the marker, so the child
        // can never block on a full stdout pipe later in the test.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains("ready") {
                    let _ = ready.send(());
                }
            }
        });

        is_ready
            .recv_timeout(READY_TIMEOUT)
            .expect("the beam never reported that it was running");
        child
    }

    fn interrupt(child: &Child) {
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).unwrap();
    }

    /// The first signal cancels the run: Alba announces it, terminates the
    /// command it started, exits on its own rather than being killed, and
    /// reports 130 — an interrupted run must not look like a success, or
    /// `alba run deploy && ship` would ship anyway.
    #[test]
    fn first_interrupt_cancels_the_run_and_exits_130() {
        let dir = project("beam slow { run \"echo ready; sleep 30\" }\n");
        let child = spawn_running(&dir, "slow");

        interrupt(&child);
        let output = child.wait_with_output().unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("cancelling"),
            "the first signal must announce the cancellation, got:\n{stderr}"
        );
        assert_eq!(
            output.status.code(),
            Some(130),
            "an interrupted run must not report success, stderr:\n{stderr}"
        );
    }

    /// The second signal aborts immediately, without waiting for the
    /// executor's grace period. The beam installs its `SIGTERM`-ignoring
    /// trap *before* reporting ready and then loops for a bounded time, so
    /// the run is provably still cancelling (inside the executor's 5 s
    /// grace) when the second signal lands, and the shell cannot outlive
    /// the test even though the abort leaves it running.
    #[test]
    fn second_interrupt_aborts_the_process() {
        let dir = project(
            "beam stubborn { run \"trap '' TERM; echo ready; i=0; while [ $i -lt 40 ]; do sleep 0.25; i=$((i+1)); done\" }\n",
        );
        let child = spawn_running(&dir, "stubborn");

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
