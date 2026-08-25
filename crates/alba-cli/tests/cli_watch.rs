//! End-to-end tests for `alba run --watch`: a real long-running process,
//! observed through `--log-format json` (the deterministic oracle) on a
//! reader thread. Commands are `echo`-only so macOS, Linux, and Windows
//! behave identically; the exit-code-on-SIGINT test is `#[cfg(unix)]`
//! like the existing interrupt tests.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct WatchProcess {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
    /// Everything the session wrote to stderr, drained by its own thread.
    /// Alba's commentary — the status lines, the diagnostics, and the
    /// reasons a session cannot start at all — all go there, so a failure
    /// here has to report it: a session that dies on `cannot start the
    /// file watcher` and one that genuinely hangs both look like an empty
    /// stdout otherwise.
    errors: Arc<Mutex<String>>,
}

impl WatchProcess {
    fn spawn(dir: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("alba"))
            .current_dir(dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

        // Read into a shared buffer rather than a channel: stderr is never
        // waited on, only reported, and a blocking read on a live session
        // would otherwise pin the buffer at whatever was flushed last.
        let stderr = child.stderr.take().unwrap();
        let errors = Arc::new(Mutex::new(String::new()));
        std::thread::spawn({
            let errors = Arc::clone(&errors);
            move || {
                let mut reader = BufReader::new(stderr);
                let mut chunk = [0u8; 1024];
                while let Ok(read) = reader.read(&mut chunk) {
                    if read == 0 {
                        break;
                    }
                    errors
                        .lock()
                        .unwrap()
                        .push_str(&String::from_utf8_lossy(&chunk[..read]));
                }
            }
        });

        Self {
            child,
            lines,
            seen: Vec::new(),
            errors,
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
                Err(_) => panic!("never saw {needle:?}{}", self.context()),
            }
        }
    }

    /// The exit status, or a panic after the same 60s with the same
    /// context. Polled rather than blocked on: a session that stops
    /// honouring Ctrl-C must fail this test with a diagnostic instead of
    /// hanging the suite, and a bare `wait()` cannot be rescued by `Drop`
    /// — that only kills the process once `wait()` has already returned.
    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the session never exited{}", self.context())
    }

    /// Both streams, for a panic message: what the session printed, and
    /// what it complained about.
    fn context(&self) -> String {
        format!(
            "\nstdout so far:\n{}\nstderr so far:\n{}",
            self.seen.join("\n"),
            self.errors.lock().unwrap()
        )
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

/// [`watch_project`]'s tree, committed, with a second beam that reacts to a
/// `docs/**` change nothing has made yet — the file `--affected` sessions
/// pick up once it moves.
fn git_watch_project() -> tempfile::TempDir {
    let dir = watch_project();
    std::fs::write(
        dir.path().join("Beamfile"),
        "default hello\n\
         beam hello { inputs [\"src/**\"] run \"echo greeting-ran\" }\n\
         beam other { inputs [\"docs/**\"] run \"echo other-ran\" }\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("docs/a"), "one").unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "alba@example.com"]);
    git(dir.path(), &["config", "user.name", "Alba"]);
    git(dir.path(), &["config", "commit.gpgsign", "false"]);
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
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

    // The opening quote is the whole point: a path that came back absolute
    // reads `"/private/var/.../src/input.txt"` and satisfies a substring
    // check on `src/input.txt` while breaking the root-relative promise the
    // event and the status line both make. Only this path is asserted, not
    // the whole array — a debounced batch coalesces whatever the operating
    // system reported in its window, and macOS routinely folds in the
    // Beamfile alongside.
    let triggered = alba.wait_for(r#""event":"watch_triggered""#);
    assert!(triggered.contains(r#""src/input.txt""#), "got: {triggered}");
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

    let status = alba.wait_for_exit();
    assert_eq!(status.code(), Some(0));
}

/// `--affected` composes with `--watch`: the reference is fixed, and every
/// run in the session recomputes its targets against the moving working
/// tree.
#[test]
fn an_affected_session_picks_up_a_beam_once_its_input_changes() {
    let dir = git_watch_project();
    let mut session = WatchProcess::spawn(
        dir.path(),
        &[
            "run",
            "--affected",
            "HEAD",
            "--watch",
            "--log-format",
            "json",
            "--no-ui",
        ],
    );
    let started = session.wait_for("run_started");
    assert!(started.contains("\"targets\":[]"), "{started}");
    session.wait_for("watch_waiting");
    std::fs::write(dir.path().join("docs/a"), "v2").unwrap();
    session.wait_for("watch_triggered");
    let started = session.wait_for("run_started");
    assert!(started.contains("\"targets\":[\"other\"]"), "{started}");
    session.wait_for("other-ran");
}
