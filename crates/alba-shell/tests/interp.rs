use std::path::PathBuf;

use alba_shell::{ShellEnv, ShellStream, execute, parse};
use tokio_util::sync::CancellationToken;

/// Runs `src` in `cwd` and returns (exit code, output lines).
async fn run_in(src: &str, cwd: PathBuf) -> (i32, Vec<(ShellStream, String)>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse(src).expect("parse");
    let env = ShellEnv {
        env: std::env::vars().collect(),
        cwd,
        output: tx,
        cancel: CancellationToken::new(),
    };
    let result = execute(&program, env).await;
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push((line.stream, line.text));
    }
    (result.exit_code, lines)
}

async fn run(src: &str) -> (i32, Vec<(ShellStream, String)>) {
    run_in(src, std::env::current_dir().unwrap()).await
}

fn stdout(lines: &[(ShellStream, String)]) -> Vec<&str> {
    lines
        .iter()
        .filter(|(s, _)| *s == ShellStream::Stdout)
        .map(|(_, t)| t.as_str())
        .collect()
}

/// Like `run`, but with extra entries appended to the starting
/// environment — for tests that need a known baseline value for a
/// variable an assignment prefix is about to temporarily override.
async fn run_with_env(
    src: &str,
    extra_env: Vec<(String, String)>,
) -> (i32, Vec<(ShellStream, String)>) {
    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.extend(extra_env);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse(src).expect("parse");
    let env = ShellEnv {
        env,
        cwd: std::env::current_dir().unwrap(),
        output: tx,
        cancel: CancellationToken::new(),
    };
    let result = execute(&program, env).await;
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push((line.stream, line.text));
    }
    (result.exit_code, lines)
}

/// A command that prints the value of environment variable `name` to
/// stdout, run through a real external binary (`sh`/`powershell`) so the
/// value observed is whatever that child's process environment actually
/// contains — not anything alba-shell itself might report. Spawning a
/// second external process as the thing being tested keeps this
/// independent of any of alba-shell's own builtins (none of which print
/// a variable in this task).
#[cfg(windows)]
fn print_env_command(name: &str) -> String {
    format!("powershell -NoProfile -Command 'echo $env:{name}'")
}

#[cfg(not(windows))]
fn print_env_command(name: &str) -> String {
    format!("sh -c 'echo ${name}'")
}

/// A command that sleeps for `seconds`, run through a real external
/// binary present by default on every platform this crate targets.
#[cfg(windows)]
fn sleep_seconds(seconds: u32) -> String {
    format!("powershell -NoProfile -Command 'Start-Sleep -Seconds {seconds}'")
}

#[cfg(not(windows))]
fn sleep_seconds(seconds: u32) -> String {
    format!("sleep {seconds}")
}

#[tokio::test]
async fn true_succeeds_and_false_fails() {
    assert_eq!(run("true").await.0, 0);
    assert_eq!(run("false").await.0, 1);
}

#[tokio::test]
async fn an_empty_command_succeeds() {
    assert_eq!(run("").await.0, 0);
}

#[tokio::test]
async fn sequencing_returns_the_last_exit_code() {
    assert_eq!(run("false; true").await.0, 0);
    assert_eq!(run("true; false").await.0, 1);
}

#[tokio::test]
async fn and_or_short_circuit() {
    assert_eq!(run("false && exit 3").await.0, 1);
    assert_eq!(run("true || exit 3").await.0, 0);
    assert_eq!(run("false || true").await.0, 0);
}

#[tokio::test]
async fn negation_inverts_the_exit_code() {
    assert_eq!(run("! false").await.0, 0);
    assert_eq!(run("! true").await.0, 1);
}

#[tokio::test]
async fn exit_stops_the_program_with_its_code() {
    let (code, lines) = run("exit 7; pwd").await;
    assert_eq!(code, 7);
    assert!(stdout(&lines).is_empty(), "nothing may run after exit");
}

#[tokio::test]
async fn pwd_prints_the_shell_cwd_and_cd_moves_it() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let (code, lines) = run_in("cd sub && pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    let printed = stdout(&lines).join("");
    assert!(printed.ends_with("sub"), "got: {printed}");
}

#[tokio::test]
async fn cd_updates_the_exported_pwd_variable() {
    // `PWD` is exported, so a child process must see it agree with
    // where `cd` actually moved to, not the directory the run started
    // in — otherwise a spawned child's own `current_dir` and its
    // inherited `PWD` would contradict each other.
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let read = print_env_command("PWD");
    let src = format!("cd sub && {read}");
    let (code, lines) = run_in(&src, dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    let printed = stdout(&lines).join("");
    assert!(printed.ends_with("sub"), "got: {printed}");
}

#[tokio::test]
async fn cd_to_a_missing_directory_fails_with_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cd nowhere", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t.contains("nowhere"))
    );
}

#[tokio::test]
async fn an_unknown_command_reports_127_with_a_message() {
    let (code, lines) = run("definitely-not-a-command-alba").await;
    assert_eq!(code, 127);
    assert!(lines.iter().any(|(s, t)| *s == ShellStream::Stderr
        && t.contains("command not found: definitely-not-a-command-alba")));
}

#[tokio::test]
async fn a_near_miss_of_a_builtin_gets_a_suggestion() {
    // `pdw` is distance 2 from `pwd`, which exists from this task on
    // (`echo` only lands in Task 6, so it cannot anchor this test yet).
    let (_, lines) = run("pdw").await;
    assert!(lines.iter().any(|(_, t)| t.contains("did you mean `pwd`?")));
}

#[tokio::test]
async fn runs_an_external_command_and_streams_its_output() {
    // cargo is guaranteed present: this workspace builds with it.
    let (code, lines) = run("cargo --version").await;
    assert_eq!(code, 0);
    assert!(stdout(&lines).iter().any(|l| l.starts_with("cargo ")));
}

#[tokio::test]
async fn a_failing_external_reports_its_exit_code() {
    let (code, _) = run("cargo definitely-not-a-subcommand").await;
    assert_ne!(code, 0);
}

#[tokio::test]
async fn cancellation_stops_a_running_external() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse("cargo --version; cargo --version").unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let env = ShellEnv {
        env: std::env::vars().collect(),
        cwd: std::env::current_dir().unwrap(),
        output: tx,
        cancel,
    };
    let started = std::time::Instant::now();
    let result = execute(&program, env).await;
    assert_eq!(result.exit_code, 130);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

#[tokio::test]
async fn an_assignment_alone_succeeds_silently() {
    let (code, lines) = run("GREETING=hello").await;
    assert_eq!(code, 0);
    assert!(lines.is_empty());
}

#[tokio::test]
async fn export_rejects_an_invalid_name() {
    let (code, lines) = run("export 1BAD=x").await;
    assert_eq!(code, 2);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn an_assignment_prefix_exports_the_variable_for_that_command_only() {
    let read = print_env_command("ALBA_TEST_OVERLAY");
    // Same shell state across both halves of the `;`: the first command
    // gets the overlay, the second must see the pre-existing value again.
    let src = format!("ALBA_TEST_OVERLAY=overlaid {read}; {read}");
    let (code, lines) = run_with_env(
        &src,
        vec![("ALBA_TEST_OVERLAY".to_string(), "original".to_string())],
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(
        stdout(&lines),
        vec!["overlaid", "original"],
        "the prefix must be visible to the one command it prefixes, and \
         restored to the prior value for the next"
    );
}

#[tokio::test]
async fn a_duplicate_assignment_prefix_restores_the_original_value() {
    // Regression test: two prefixes assigning the same name (`FOO=1
    // FOO=2 cmd`) must still restore the value from *before either one*
    // ran, not the intermediate `FOO=1`.
    let read = print_env_command("ALBA_TEST_DUP_OVERLAY");
    let src = format!("ALBA_TEST_DUP_OVERLAY=one ALBA_TEST_DUP_OVERLAY=two {read}; {read}");
    let (code, lines) = run_with_env(
        &src,
        vec![("ALBA_TEST_DUP_OVERLAY".to_string(), "original".to_string())],
    )
    .await;
    assert_eq!(code, 0);
    assert_eq!(stdout(&lines), vec!["two", "original"]);
}

#[tokio::test]
async fn cancellation_kills_a_running_external_mid_flight() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse(&sleep_seconds(30)).unwrap();
    let cancel = CancellationToken::new();
    let env = ShellEnv {
        env: std::env::vars().collect(),
        cwd: std::env::current_dir().unwrap(),
        output: tx,
        cancel: cancel.clone(),
    };

    // Unlike `cancellation_stops_a_running_external` (cancelled before
    // `execute` even starts, so it never reaches `spawn::run_external`),
    // this cancels a genuinely running external mid-flight, exercising
    // the termination escalation itself.
    let handle = tokio::spawn(async move { execute(&program, env).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let cancelled_at = std::time::Instant::now();
    cancel.cancel();

    let result = handle.await.expect("execute task panicked");
    let elapsed = cancelled_at.elapsed();

    assert_eq!(result.exit_code, 130);
    // `execute()` cannot return before the child has actually exited
    // (`spawn::run_external` always awaits its exit status, whether that
    // comes from natural completion or from `terminate`), so a prompt
    // return here is direct evidence the process actually died — not
    // merely that the shell gave up waiting on it. A 30s sleep only
    // exits this fast if the graceful-stop signal (unix `SIGTERM`,
    // windows `TerminateProcess` via `start_kill`) actually killed it;
    // 3s leaves headroom over that near-instant death while still
    // failing loudly if the 5s grace-period timeout had to fire instead.
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "execute() took {elapsed:?} to return after cancellation; expected \
         the graceful-stop signal to kill the sleeping process almost \
         immediately, not the 5s grace period or the full 30s sleep"
    );
}

#[tokio::test]
async fn an_explicit_path_that_does_not_exist_reports_command_not_found() {
    // A path separator routes lookup away from PATH search and straight
    // to a cwd-relative resolution (see `run_external_command`); a
    // missing file there is "not found" (127), not a spawn failure
    // (126) — the frozen semantics distinguish the two.
    let (code, lines) = run("./this-path-does-not-exist-alba-test-xyz").await;
    assert_eq!(code, 127);
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t.contains("command not found"))
    );
}

// Building an absolute-path binary literally named after a builtin
// (`true`) needs an executable file, which only a POSIX shebang script
// gives us portably without compiling anything; a windows equivalent
// would need a real PE binary or a `.bat`/`.cmd` extension that breaks
// the "same name as the builtin" premise the test is pinning down.
#[cfg(unix)]
#[tokio::test]
async fn an_absolute_path_bypasses_the_builtin_of_the_same_name() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("true");
    std::fs::write(&script, "#!/bin/sh\necho real-true\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let src = script.display().to_string();
    let (code, lines) = run(&src).await;
    assert_eq!(code, 0);
    assert_eq!(
        stdout(&lines),
        vec!["real-true"],
        "an explicit path must run the external script, not the silent `true` builtin"
    );
}
