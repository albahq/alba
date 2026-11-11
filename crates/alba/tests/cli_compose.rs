//! End-to-end tests for the compose executor, driven through the real `alba`
//! binary against a real docker daemon with the Compose plugin. See
//! `cli_docker.rs` for the conventions this file follows.
//!
//! Every test is `#[ignore]`d because it requires `docker` on the `PATH` and
//! a running daemon; run them explicitly with
//! `cargo test -p alba --test cli_compose -- --ignored`.
//!
//! Every test works in its own throwaway Compose project ([`ComposeProject`]),
//! named after the test and the process id, so it never collides with another
//! test, another run, or a developer's own project. Its beams carry the same
//! token, so the `alba.beam` label of their containers identifies them.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn alba() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("alba").unwrap()
}

/// A throwaway Compose project in a temp directory, with `compose.yaml` at
/// its root. Dropping it runs `docker compose --profile "*" down -v --rmi
/// local` on that project only, and removes every container labelled for
/// one of its beams, even when the test panics.
struct ComposeProject {
    dir: tempfile::TempDir,
    token: String,
}

impl ComposeProject {
    /// `yaml` is the file's content without its `name:` line.
    fn new(suffix: &str, yaml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let token = format!("cli_compose_it_{suffix}_{}", std::process::id());
        let name = token.replace('_', "-");
        std::fs::write(
            dir.path().join("compose.yaml"),
            format!("name: {name}\n{yaml}"),
        )
        .unwrap();
        Self { dir, token }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A beam name unique to this test and this process. Beam identifiers
    /// only allow letters, digits and underscores.
    fn beam(&self, which: &str) -> String {
        format!("{}_{which}", self.token)
    }

    /// Runs `docker compose <args>` in the project's root.
    fn compose(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new("docker")
            .current_dir(self.path())
            .arg("compose")
            .args(args)
            .output()
            .unwrap()
    }

    /// The id of `service`'s container, running or not.
    fn container_id(&self, service: &str) -> Option<String> {
        let output = self.compose(&["ps", "-a", "-q", service]);
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!id.is_empty()).then_some(id)
    }

    /// The ids of the containers labelled for one of this test's beams.
    fn beam_containers(&self) -> Vec<String> {
        beam_containers_matching(&self.token)
    }

    /// Writes `content` at `relative` below the project's root, creating the
    /// directories on the way, and returns the file's directory.
    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.path().join(relative);
        let parent = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&path, content).unwrap();
        parent
    }
}

impl Drop for ComposeProject {
    fn drop(&mut self) {
        let _ = self.compose(&["--profile", "*", "down", "-v", "--rmi", "local"]);
        for id in beam_containers_matching(&self.token) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &id])
                .output();
        }
    }
}

/// The ids of the containers whose `alba.beam` label contains `token`
/// (an imported beam's label is prefixed by its namespace).
fn beam_containers_matching(token: &str) -> Vec<String> {
    let output = std::process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=alba.beam",
            "--format",
            "{{.ID}} {{.Label \"alba.beam\"}}",
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, beam)| beam.contains(token))
        .map(|(id, _)| id.to_string())
        .collect()
}

/// The service every test but the dependency ones runs in: the project is
/// bound on `/app`, which is what maps a beam's directory into the container.
const APP_SERVICE: &str = "services:\n  php:\n    image: alpine:3\n    volumes:\n      - .:/app\n";

/// A module two directories below the Compose file runs in the root's
/// service, from `alba run` in the module and from the importing root.
#[test]
#[ignore = "requires a running docker daemon"]
fn a_module_two_directories_down_runs_in_the_root_service() {
    let project = ComposeProject::new("module", APP_SERVICE);
    let beam = project.beam("pwd");
    let module = project.write(
        "modules/billing/Beamfile",
        &format!(
            "beam {beam} {{\n  executor compose {{ service \"php\" file \"../../compose.yaml\" }}\n  run \"pwd\"\n}}\n"
        ),
    );
    project.write(
        "Beamfile",
        "import \"modules/billing/Beamfile\" as billing\n",
    );

    alba()
        .current_dir(&module)
        .args(["run", &beam])
        .assert()
        .success()
        .stdout(predicates::str::contains("/app/modules/billing"));

    alba()
        .current_dir(project.path())
        .args(["run", &format!("billing:{beam}")])
        .assert()
        .success()
        .stdout(predicates::str::contains("/app/modules/billing"));
}

/// Beams on one service do not wait for one another: each gets a container
/// of its own.
#[test]
#[ignore = "requires a running docker daemon"]
fn beams_on_one_service_run_in_parallel() {
    let project = ComposeProject::new("parallel", APP_SERVICE);
    let (first, second, all) = (
        project.beam("first"),
        project.beam("second"),
        project.beam("all"),
    );
    project.write(
        "Beamfile",
        &format!(
            "beam {first} {{ executor compose {{ service \"php\" }} run \"sleep 2\" }}\n\
             beam {second} {{ executor compose {{ service \"php\" }} run \"sleep 2\" }}\n\
             beam {all} {{ needs [{first}, {second}] run \"echo done\" }}\n"
        ),
    );

    let started = Instant::now();
    alba()
        .current_dir(project.path())
        .args(["run", &all])
        .assert()
        .success();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "two 2 s beams on one service took {elapsed:?}, they did not run in parallel"
    );
}

const WITH_DATABASE: &str = "services:\n  php:\n    image: alpine:3\n    volumes:\n      - .:/app\n    depends_on:\n      - db\n  db:\n    image: alpine:3\n    command: sleep 300\n";

/// A run puts everything back: no container of a beam stays, and the
/// dependency the run started is gone.
#[test]
#[ignore = "requires a running docker daemon"]
fn a_run_leaves_no_container_and_no_dependency_it_started() {
    let project = ComposeProject::new("clean", WITH_DATABASE);
    let beam = project.beam("clean");
    project.write(
        "Beamfile",
        &format!("beam {beam} {{ executor compose {{ service \"php\" }} run \"echo ran\" }}\n"),
    );

    alba()
        .current_dir(project.path())
        .args(["run", &beam])
        .assert()
        .success()
        .stdout(predicates::str::contains("ran"));

    assert_eq!(project.beam_containers(), Vec::<String>::new());
    assert_eq!(
        project.container_id("db"),
        None,
        "the dependency the run started must be removed"
    );
}

/// A dependency that was already up before the run is not the run's to stop.
#[test]
#[ignore = "requires a running docker daemon"]
fn a_dependency_already_running_survives_the_run() {
    let project = ComposeProject::new("survive", WITH_DATABASE);
    let beam = project.beam("survive");
    project.write(
        "Beamfile",
        &format!("beam {beam} {{ executor compose {{ service \"php\" }} run \"echo ran\" }}\n"),
    );
    let up = project.compose(&["up", "-d", "db"]);
    assert!(
        up.status.success(),
        "{}",
        String::from_utf8_lossy(&up.stderr)
    );
    let before = project.container_id("db").expect("db is up");

    alba()
        .current_dir(project.path())
        .args(["run", &beam])
        .assert()
        .success();

    assert_eq!(project.container_id("db"), Some(before));
    assert_eq!(project.beam_containers(), Vec::<String>::new());
}
