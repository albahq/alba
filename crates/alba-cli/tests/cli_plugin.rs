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

/// `PATH` with a directory holding *only* `alba-executor-example`
/// prepended, so `alba` resolves it the way a real user install would —
/// by finding a plugin binary on the `PATH` with nothing else beside it.
/// `target/debug` itself would not do: it also holds `alba`, every other
/// test binary, and anything else this workspace produces, so prepending
/// it directly would leave the resolution untested against exactly the
/// kind of same-named collision a real plugin install never has to
/// contend with.
///
/// `cargo_bin` resolves a path already built into the workspace's target
/// directory (it does not build anything itself), which is why the full
/// gate matters here: `cargo test --workspace` builds every workspace
/// binary, `alba-executor-example` included, so by the time this runs the
/// file is already on disk to link into the isolated directory built
/// below.
///
/// The returned `TempDir` must be kept alive by the caller for as long as
/// the `PATH` is in use: it owns the directory the child process resolves
/// the plugin from, and dropping it early deletes that directory (and,
/// via `symlink`, the name the child would look up) out from under a
/// still-running `alba`.
fn plugin_path() -> (std::ffi::OsString, tempfile::TempDir) {
    let example = assert_cmd::cargo::cargo_bin("alba-executor-example");
    let name = example
        .file_name()
        .expect("a binary path always has a file name");

    let isolated = tempfile::tempdir().unwrap();
    let linked = isolated.path().join(name);
    // A symlink is enough on unix, where resolving `PATH` entries follows
    // them like any other file; windows has no equivalent unprivileged
    // symlink available by default, so a plain copy keeps this portable
    // without reaching for elevated permissions just for a test.
    #[cfg(unix)]
    std::os::unix::fs::symlink(&example, &linked).unwrap();
    #[cfg(windows)]
    std::fs::copy(&example, &linked).unwrap();

    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![isolated.path().to_path_buf()];
    dirs.extend(std::env::split_paths(&existing));
    let path = std::env::join_paths(dirs).expect("the augmented PATH must join into one OsString");
    (path, isolated)
}

/// A beam wired to `executor example` runs its command through the
/// spawned plugin process end to end: `alba` resolves the binary, speaks
/// the handshake, sends `execute`, and relays the plugin's `output` line
/// back into the beam's own stdout.
#[test]
fn a_plugin_beam_runs_end_to_end() {
    let dir =
        project("beam hello { executor example run \"echo carried by the example plugin\" }\n");
    let (path, _plugin_dir) = plugin_path();

    alba()
        .current_dir(&dir)
        .env("PATH", path)
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
    let (path, _plugin_dir) = plugin_path();

    alba()
        .current_dir(&dir)
        .env("PATH", path)
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

/// `alba plugin check` drives the reference plugin through the whole
/// protocol — handshake, an execute, a cancel on a fresh session, and that
/// session's close — and reports every check as passing.
#[test]
fn plugin_check_passes_the_example_plugin() {
    let example = assert_cmd::cargo::cargo_bin("alba-executor-example");

    alba()
        .args([
            "plugin",
            "check",
            example.to_str().expect("a utf-8 target path"),
            "--command",
            "echo probe",
            "--cancel-command",
            "sleep 30000",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{2713} handshake"))
        .stdout(predicates::str::contains("\u{2713} execute"))
        .stdout(predicates::str::contains("\u{2713} cancel"))
        .stdout(predicates::str::contains("answered in"))
        .stdout(predicates::str::contains("\u{2713} close"))
        .stdout(predicates::str::contains("conformant"));
}

/// `--cancel-command`'s own default (no flag passed at all) must actually
/// outlive the check's 100ms delay before it sends `cancel`: regression for
/// a default that finished on its own well before `cancel` was ever sent,
/// which made the cancel check pass without a `cancel` message ever having
/// crossed the wire (see `commands::plugin`'s `CANCEL_AFTER`/lower-bound
/// doc comments).
#[test]
fn plugin_check_default_cancel_command_actually_exercises_cancel() {
    let example = assert_cmd::cargo::cargo_bin("alba-executor-example");

    alba()
        .args([
            "plugin",
            "check",
            example.to_str().expect("a utf-8 target path"),
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{2713} cancel"))
        .stdout(predicates::str::contains("answered in"))
        .stdout(predicates::str::contains("conformant"));
}

/// A `--cancel-command` that finishes on its own before the check ever
/// sends `cancel` (100ms in) cannot possibly demonstrate answering it: the
/// lower-bound check rejects it outright instead of reporting a pass that
/// exercised nothing.
#[test]
fn plugin_check_rejects_a_cancel_command_that_finishes_before_cancel_is_sent() {
    let example = assert_cmd::cargo::cargo_bin("alba-executor-example");

    alba()
        .args([
            "plugin",
            "check",
            example.to_str().expect("a utf-8 target path"),
            "--cancel-command",
            "sleep 5",
        ])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("\u{2717} cancel"))
        .stdout(predicates::str::contains("before"))
        .stdout(predicates::str::contains("not conformant"));
}

/// A beam declaring no options at all must still see `options` as an empty
/// JSON object on the wire, never `null` — the guarantee
/// `docs/plugin-protocol.md` makes and the engine itself honours. `alba
/// plugin check` is the one tool meant to give a third-party plugin author
/// confidence in that guarantee, so it must honour it too: this script
/// refuses the handshake unless it sees `"options":{}` verbatim.
#[cfg(unix)]
#[test]
fn plugin_check_sends_empty_options_not_null() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("options-shape-executor");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"options":{}'*)
      printf '%s\n' '{"type":"ready"}'
      ;;
    *'"type":"open"'*)
      printf '%s\n' '{"type":"error","message":"expected options as {}"}'
      exit 1
      ;;
    *'"command":"echo'*)
      printf '%s\n' '{"type":"exit","code":0}'
      ;;
    *'"command":"sleep'*)
      while IFS= read -r inner; do
        case "$inner" in
          *'"type":"cancel"'*)
            printf '%s\n' '{"type":"exit","code":-1}'
            break
            ;;
        esac
      done
      ;;
    *'"type":"close"'*)
      exit 0
      ;;
  esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    alba()
        .args(["plugin", "check", script.to_str().expect("a utf-8 path")])
        .assert()
        .success()
        .stdout(predicates::str::contains("\u{2713} handshake"))
        .stdout(predicates::str::contains("conformant"));
}

/// A plugin that answers the handshake and an ordinary command normally,
/// but never answers `cancel` on a long-running one, must not be reported
/// as conformant: `PluginExecutor` absorbs a plugin like this by
/// force-killing it once its own grace elapses and handing back the same
/// `Ok` a prompt answer would, so `Ok`/`Err` alone cannot catch it — only
/// how long the answer took can (see `cancel_and_close_check`'s doc
/// comment in `commands/plugin.rs`). `alba-executors` ships a scripted
/// fake plugin with exactly this "answers everything but never reacts to
/// cancel" shape (`fake-plugin`'s `deaf` mode), but its behavior is
/// selected by an argv the wire protocol has no room to carry (see
/// `PluginExecutor::with_args`'s doc comment) — and `deaf` ignores every
/// message after the handshake, not just `cancel`, which would also fail
/// this test's own execute check and make the whole run pay the 30s
/// execute timeout on top of the cancel grace. A small script fixture
/// keeps the two checks independent and the test fast: it answers `open`
/// and any `echo` command immediately, and silently ignores everything
/// else (a long-running command's `execute`, and the `cancel` sent for
/// it) — the shape `commands/plugin.rs`'s own doc comment describes.
#[cfg(unix)]
#[test]
fn plugin_check_reports_a_cancel_deaf_plugin() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cancel-deaf-executor");
    std::fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"open"'*)
      printf '%s\n' '{"type":"ready"}'
      ;;
    *'"command":"echo'*)
      printf '%s\n' '{"type":"exit","code":0}'
      ;;
    *'"type":"close"'*)
      exit 0
      ;;
  esac
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    alba()
        .args([
            "plugin",
            "check",
            script.to_str().expect("a utf-8 path"),
            "--command",
            "echo probe",
            "--cancel-command",
            "sleep 30000",
        ])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("\u{2713} handshake"))
        .stdout(predicates::str::contains("\u{2713} execute"))
        .stdout(predicates::str::contains("\u{2717} cancel"))
        .stdout(predicates::str::contains("not conformant"));
}

/// A binary that never answers the handshake fails the check outright: no
/// process left running past the check's own handshake timeout, and a
/// clean non-conformant report rather than a hang.
#[cfg(unix)]
#[test]
fn plugin_check_reports_a_mute_binary() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("mute-executor");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    alba()
        .args(["plugin", "check", script.to_str().expect("a utf-8 path")])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("\u{2717} handshake"));
}

/// A binary path that does not exist at all is Alba's own error, not a
/// verdict on some plugin's conformance — reported as exit 2, the same
/// code every other "Alba itself failed" case uses.
#[test]
fn plugin_check_rejects_a_missing_binary_as_an_alba_error() {
    alba()
        .args(["plugin", "check", "/no/such/binary"])
        .assert()
        .code(2);
}
