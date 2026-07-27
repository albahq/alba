//! Integration tests for [`alba_executors::SystemShellExecutor`].
//!
//! Command strings are built through small platform-conditional helpers so
//! the same test bodies exercise `sh -c` on unix and `powershell -NoProfile
//! -Command` on windows.

use alba_executors::{CommandSpec, ExecContext, Executor, SystemShellExecutor};

#[cfg(windows)]
fn exit_with_code(code: i32) -> String {
    format!("exit {code}")
}

#[cfg(not(windows))]
fn exit_with_code(code: i32) -> String {
    format!("exit {code}")
}

#[cfg(windows)]
fn print_env_var(name: &str) -> String {
    format!("echo $env:{name}")
}

#[cfg(not(windows))]
fn print_env_var(name: &str) -> String {
    format!("echo ${name}")
}

#[cfg(windows)]
fn sleep_seconds(seconds: u32) -> String {
    format!("Start-Sleep -Seconds {seconds}")
}

#[cfg(not(windows))]
fn sleep_seconds(seconds: u32) -> String {
    format!("sleep {seconds}")
}

#[tokio::test]
async fn runs_command_and_streams_lines() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = SystemShellExecutor
        .execute(
            CommandSpec {
                command: "echo hello".into(),
                env: vec![],
                cwd: std::env::current_dir().unwrap(),
            },
            ExecContext {
                output: tx,
                cancel: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(rx.recv().await.unwrap().text, "hello");
}

#[tokio::test]
async fn propagates_exit_code_and_env() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let result = SystemShellExecutor
        .execute(
            CommandSpec {
                command: exit_with_code(3),
                env: vec![],
                cwd: std::env::current_dir().unwrap(),
            },
            ExecContext {
                output: tx,
                cancel: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 3);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let result = SystemShellExecutor
        .execute(
            CommandSpec {
                command: print_env_var("ALBA_TEST_VAR"),
                env: vec![("ALBA_TEST_VAR".into(), "hi-there".into())],
                cwd: std::env::current_dir().unwrap(),
            },
            ExecContext {
                output: tx,
                cancel: Default::default(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(rx.recv().await.unwrap().text, "hi-there");
}

#[tokio::test]
async fn cancellation_terminates_child() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = tokio_util::sync::CancellationToken::new();

    let cancel_clone = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel_clone.cancel();
    });

    let start = tokio::time::Instant::now();
    let result = SystemShellExecutor
        .execute(
            CommandSpec {
                command: sleep_seconds(30),
                env: vec![],
                cwd: std::env::current_dir().unwrap(),
            },
            ExecContext { output: tx, cancel },
        )
        .await
        .unwrap();
    let elapsed = start.elapsed();

    // exit_code is platform/signal dependent (a killed process does not
    // exit with 0); what matters is that execute() actually returned.
    let _ = result.exit_code;

    // A bare "< 6s" bound would still pass even if the graceful-stop signal
    // did nothing at all and execute() only ever returned via the 5s grace
    // timeout's forceful escalation — that would prove nothing about
    // SIGTERM/kill actually working. `sleep 30` dies essentially
    // immediately once signalled, so the real, working path returns in
    // well under a second; 2s leaves generous headroom for a loaded CI
    // machine while still failing loudly if the grace period had to be
    // exhausted.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "execute() took {elapsed:?} to return after cancellation \
         (>= 5s would mean the grace-period timeout fired instead of \
         the process dying from the graceful-stop signal)"
    );
    // Hard ceiling matching the documented contract: signal, 5s grace,
    // then force-kill. Even in the worst case this must still hold.
    assert!(
        elapsed < std::time::Duration::from_secs(6),
        "execute() took {elapsed:?}, exceeding the 5s grace period plus overhead"
    );
}
