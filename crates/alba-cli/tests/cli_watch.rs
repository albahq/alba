//! End-to-end tests for `alba run --watch`: a real long-running process,
//! observed through `--log-format json` (the deterministic oracle) on a
//! reader thread. Commands are `echo`-only so macOS, Linux, and Windows
//! behave identically; the exit-code-on-SIGINT test is `#[cfg(unix)]`
//! like the existing interrupt tests.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

struct WatchProcess {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl WatchProcess {
    fn spawn(dir: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("alba"))
            .current_dir(dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Reads lines until one contains `needle`, or panics after 60s with
    /// everything seen so far — a hung watch must fail with context.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "never saw {needle:?}; output so far:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }
}

impl Drop for WatchProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn watch_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/input.txt"), "one").unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "default hello\n\
         beam hello { inputs [\"src/**\"] run \"echo greeting-ran\" }\n",
    )
    .unwrap();
    dir
}

/// The full cycle: initial run, waiting, a real file change, a
/// triggered second run.
#[test]
fn a_file_change_triggers_a_second_run() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(
        dir.path(),
        &["run", "--watch", "hello", "--log-format", "json"],
    );

    alba.wait_for(r#""event":"run_finished""#);
    alba.wait_for(r#""event":"watch_waiting""#);

    std::fs::write(dir.path().join("src/input.txt"), "two").unwrap();

    let triggered = alba.wait_for(r#""event":"watch_triggered""#);
    assert!(triggered.contains("src/input.txt"), "got: {triggered}");
    alba.wait_for(r#""event":"run_finished""#);
    alba.wait_for(r#""event":"watch_waiting""#);
}

/// `--watch` composes with the default beam: no beam named on the
/// command line.
#[test]
fn watch_runs_the_default_beam() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(dir.path(), &["run", "--watch", "--log-format", "json"]);
    alba.wait_for(r#""beam":"hello""#);
}

/// Editing the Beamfile mid-session is a reload, not a stale re-run:
/// the next run carries the new command's output.
#[test]
fn a_beamfile_edit_reloads_the_project() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(
        dir.path(),
        &["run", "--watch", "hello", "--log-format", "json"],
    );
    alba.wait_for(r#""event":"watch_waiting""#);

    std::fs::write(
        dir.path().join("Beamfile"),
        "default hello\n\
         beam hello { inputs [\"src/**\"] run \"echo greeting-edited\" }\n",
    )
    .unwrap();

    alba.wait_for(r#""event":"watch_triggered""#);
    alba.wait_for("greeting-edited");
}

/// Ctrl-C ends the session with exit code 0: the runs already reported
/// themselves, and an orderly goodbye is not a failure.
#[cfg(unix)]
#[test]
fn sigint_ends_the_session_with_code_0() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(
        dir.path(),
        &["run", "--watch", "hello", "--log-format", "json"],
    );
    alba.wait_for(r#""event":"watch_waiting""#);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(alba.child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();

    let status = alba.child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}
