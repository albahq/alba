//! End-to-end tests for the docker executor, driven through the real `alba`
//! binary against a real docker daemon. See `cli_run.rs` for the ordinary
//! (non-docker) end-to-end tests and its tempdir + `assert_cmd` conventions,
//! which this file follows.
//!
//! Every test is `#[ignore]`d — see the reason string on each — because they
//! require `docker` on the `PATH` and a running daemon; run them explicitly
//! with `cargo test -p alba-cli --test cli_docker -- --ignored`.
//!
//! Each test's beam carries a name unique to both the test and the process
//! that runs it (see `beam_name`), mirroring `alba-executors/tests/docker.rs`'s
//! discipline: a [`ContainerGuard`] removes any container carrying that
//! beam's `alba.beam` label when the test ends, panic or not, so a failing
//! assertion can never leave a container behind, and no two tests' (or two
//! runs') containers can ever be confused for one another.

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

/// A temporary directory holding `beamfile` as its `Beamfile`.
fn project(beamfile: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();
    dir
}

/// A beam name unique to both this test (`suffix`) and this run (the
/// process id): the `alba.beam` label it produces (see `run_args` in
/// `alba-executors`) must not collide with a leftover from an earlier run
/// on a shared runner. Beam identifiers only allow letters, digits, and
/// underscores, so the suffix and the pid are joined with `_`, not `-`.
fn beam_name(suffix: &str) -> String {
    format!("cli_docker_it_{suffix}_{}", std::process::id())
}

/// Unconditional cleanup for one test's container, found by its
/// `alba.beam` label rather than the (docker-generated) container name.
/// `Drop` runs during a panicking unwind, so holding one of these for the
/// duration of a test guarantees no live container survives a failed
/// assertion, regardless of where it fails.
struct ContainerGuard {
    label: String,
}

impl ContainerGuard {
    fn new(beam: &str) -> Self {
        Self {
            label: beam.to_string(),
        }
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let Ok(output) = std::process::Command::new("docker")
            .args([
                "ps",
                "-aq",
                "--filter",
                &format!("label=alba.beam={}", self.label),
            ])
            .output()
        else {
            return;
        };
        for id in String::from_utf8_lossy(&output.stdout).split_whitespace() {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", id])
                .output();
        }
    }
}

/// A beam declaring `executor docker` actually runs its command inside the
/// named image, through the real `alba` binary end to end.
#[test]
#[ignore = "requires a running docker daemon"]
fn a_docker_beam_runs_its_command_in_the_image() {
    let beam = beam_name("hello");
    let _guard = ContainerGuard::new(&beam);
    let dir = project(&format!(
        "beam {beam} {{\n  executor docker {{ image \"alpine:3\" }}\n  run \"echo hello from a container\"\n}}\n"
    ));

    alba()
        .current_dir(&dir)
        .args(["run", &beam])
        .assert()
        .success()
        .stdout(predicates::str::contains("hello from a container"));
}

/// The docker mount actually reaches the directory holding the Beamfile —
/// the same directory `alba_engine::beamfile_dir` resolves for the
/// watcher — and not some other cwd-dependent path. Mirrors
/// `alba-executors/tests/docker.rs`'s
/// `the_project_mount_makes_host_files_visible`, but exercised through the
/// real CLI end to end (`commands::run::executors`'s project-root
/// resolution included) rather than the executor directly.
#[test]
#[ignore = "requires a running docker daemon"]
fn the_docker_mount_reaches_the_beamfiles_own_directory() {
    let beam = beam_name("project_mount");
    let _guard = ContainerGuard::new(&beam);
    let dir = project(&format!(
        "beam {beam} {{\n  executor docker {{ image \"alpine:3\" }}\n  run \"cat hello.txt\"\n}}\n"
    ));
    std::fs::write(dir.path().join("hello.txt"), "hi from the host").unwrap();

    alba()
        .current_dir(&dir)
        .args(["run", &beam])
        .assert()
        .success()
        .stdout(predicates::str::contains("hi from the host"));
}

/// An image docker cannot find fails the *beam*, not the run machinery: the
/// process exits 1 (a beam failure, `RunSummary::exit_code`), never 2 (an
/// Alba error) — the same distinction `cli_run.rs`'s
/// `beam_failure_exits_1_and_cancels_dependents` pins for an ordinary
/// command failure.
#[test]
#[ignore = "requires a running docker daemon"]
fn a_missing_image_fails_the_beam_not_the_run_machinery() {
    let beam = beam_name("missing_image");
    let _guard = ContainerGuard::new(&beam);
    let dir = project(&format!(
        "beam {beam} {{\n  executor docker {{ image \"alba-definitely-does-not-exist:latest\" }}\n  run \"echo unreachable\"\n}}\n"
    ));

    alba()
        .current_dir(&dir)
        .args(["run", &beam])
        .assert()
        .code(1)
        .stdout(predicates::str::contains(beam.as_str()))
        .stderr(predicates::str::contains("1 failed"));
}
