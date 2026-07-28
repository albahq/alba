//! Integration tests for [`alba_executors::SystemShellExecutor`].
//!
//! Command strings are built through small platform-conditional helpers so
//! the same test bodies exercise `sh -c` on unix and `powershell -NoProfile
//! -Command` on windows.
//!
//! A few tests (marked `#[cfg(unix)]`) rely on POSIX shell constructs with
//! no direct windows equivalent — `trap`, backgrounding with `&`, process
//! groups — to pin down the process-group-wide cancellation contract
//! documented on `ExecContext::cancel`. Windows deliberately does not get
//! that guarantee (documented there too), so there is nothing equivalent
//! to assert on windows for those specific scenarios.

use std::time::Duration;

use alba_executors::{CommandSpec, ExecContext, Executor, SystemShellExecutor};

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

fn spec(command: impl Into<String>) -> CommandSpec {
    CommandSpec {
        command: command.into(),
        env: vec![],
        cwd: std::env::current_dir().unwrap(),
    }
}

fn ctx() -> (
    ExecContext,
    tokio::sync::mpsc::UnboundedReceiver<alba_executors::OutputLine>,
) {
    let (output, rx) = tokio::sync::mpsc::unbounded_channel();
    (
        ExecContext {
            output,
            cancel: Default::default(),
        },
        rx,
    )
}

/// Cancels `cancel` after `delay`, returning the instant it did so. Used to
/// measure cancellation-to-return latency without including command spawn
/// time (which, especially on windows' `powershell` cold start, would
/// otherwise eat into a tight timing budget for reasons unrelated to what
/// these tests actually check).
fn cancel_after(
    cancel: tokio_util::sync::CancellationToken,
    delay: Duration,
) -> tokio::sync::oneshot::Receiver<tokio::time::Instant> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let now = tokio::time::Instant::now();
        cancel.cancel();
        let _ = tx.send(now);
    });
    rx
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
    let (context, _rx) = ctx();
    let result = SystemShellExecutor
        .execute(spec(exit_with_code(3)), context)
        .await
        .unwrap();
    assert_eq!(result.exit_code, 3);

    let (context, mut rx) = ctx();
    let result = SystemShellExecutor
        .execute(
            CommandSpec {
                command: print_env_var("ALBA_TEST_VAR"),
                env: vec![("ALBA_TEST_VAR".into(), "hi-there".into())],
                cwd: std::env::current_dir().unwrap(),
            },
            context,
        )
        .await
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(rx.recv().await.unwrap().text, "hi-there");
}

#[tokio::test]
async fn cancellation_terminates_child() {
    let (context, _rx) = ctx();
    let cancel = context.cancel.clone();
    let cancelled_at = cancel_after(cancel, Duration::from_millis(100));

    let result = SystemShellExecutor
        .execute(spec(sleep_seconds(30)), context)
        .await
        .unwrap();
    let returned_at = tokio::time::Instant::now();

    // exit_code is platform/signal dependent (a killed process does not
    // exit with 0); what matters here is that execute() actually returned
    // promptly. `cancellation_delivers_a_real_sigterm` (unix) pins the
    // actual signal semantics.
    let _ = result.exit_code;

    let elapsed = returned_at - cancelled_at.await.unwrap();
    // A bare "< 6s" bound would still pass even if the graceful-stop signal
    // did nothing at all and execute() only ever returned via the 5s grace
    // timeout's forceful escalation — that would prove nothing about
    // SIGTERM/kill actually working. `sleep 30` dies essentially
    // immediately once signalled, so the real, working path returns in
    // well under a second measured from the moment cancellation fired; 2s
    // leaves generous headroom for a loaded CI machine while still failing
    // loudly if the grace period had to be exhausted.
    assert!(
        elapsed < Duration::from_secs(2),
        "execute() took {elapsed:?} to return after cancellation fired \
         (>= 5s would mean the grace-period timeout fired instead of \
         the process dying from the graceful-stop signal)"
    );
    // Hard ceiling matching the documented contract: signal, 5s grace,
    // then force-kill. Even in the worst case this must still hold.
    assert!(
        elapsed < Duration::from_secs(6),
        "execute() took {elapsed:?}, exceeding the 5s grace period plus overhead"
    );
}

/// Pins the actual signal, not just prompt termination: `child.kill()`
/// (`SIGKILL`) cannot be caught, so if `execute()` only ever force-killed
/// the child instead of sending a real `SIGTERM`, this process's own
/// signal handler would never run and the exit code would not be 42.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_delivers_a_real_sigterm() {
    let (context, _rx) = ctx();
    let cancel = context.cancel.clone();
    let _cancelled_at = cancel_after(cancel, Duration::from_millis(200));

    let result = SystemShellExecutor
        .execute(spec("trap 'exit 42' TERM; sleep 30"), context)
        .await
        .unwrap();

    assert_eq!(
        result.exit_code, 42,
        "expected the child's own SIGTERM handler to run, not a plain kill"
    );
}

/// Deliberately slow (~5s): the one branch nothing else in this suite
/// covers is the grace-period timeout actually firing and escalating from
/// `SIGTERM` to `SIGKILL`. A `trap '' TERM` process ignores the graceful
/// request outright, so the only way it dies is the forceful, group-wide
/// kill after `GRACE_PERIOD` elapses. Worth the cost: this is the riskiest
/// untested branch in the cancellation path (a bug here means a command
/// that ignores SIGTERM never gets killed at all).
///
/// Deliberately a single foreground process with no backgrounded child:
/// `cancellation_kills_the_whole_process_group` already covers group-wide
/// reach, and combining the two scenarios does not compose — a
/// backgrounded child that does not itself trap TERM would die from the
/// group-wide signal immediately, unblocking a `wait` well before the
/// parent's own trap-ignored SIGTERM could ever need to escalate, which
/// would make this test's `elapsed >= 5s` assertion false for reasons
/// unrelated to what it means to check.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_escalates_to_sigkill_after_grace_period() {
    let (context, _rx) = ctx();
    let cancel = context.cancel.clone();
    let cancelled_at = cancel_after(cancel, Duration::from_millis(200));

    let _result = SystemShellExecutor
        .execute(spec("trap '' TERM; sleep 30"), context)
        .await
        .unwrap();
    let returned_at = tokio::time::Instant::now();
    let elapsed = returned_at - cancelled_at.await.unwrap();

    assert!(
        elapsed >= Duration::from_secs(5),
        "expected the grace period to actually elapse before the kill, took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(7),
        "escalation should follow shortly after the grace period, took {elapsed:?}"
    );
}

/// Reproduces the bug a compound/backgrounding command used to hit:
/// cancelling `execute()` only signalled the immediate `sh` process, so a
/// grandchild it backgrounded (`(sleep 4; touch marker) & wait`) kept
/// running to completion and created the marker file — even though the
/// run was reported cancelled. With the child spawned as the leader of its
/// own process group and both SIGTERM/SIGKILL targeting that whole group,
/// the grandchild must die with it and the marker must never appear.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_the_whole_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");

    let (context, _rx) = ctx();
    let cancel = context.cancel.clone();
    let _cancelled_at = cancel_after(cancel, Duration::from_millis(200));

    SystemShellExecutor
        .execute(
            spec(format!("(sleep 4; touch {}) & wait", marker.display())),
            context,
        )
        .await
        .unwrap();

    // Give a would-be-surviving grandchild ample time to finish its 4s
    // sleep and create the marker before asserting it did not.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "grandchild survived cancellation and created the marker file — \
         cancellation only reached the immediate shell process, not its \
         process group"
    );
}

/// Reproduces the second bug the missing process-group scope caused,
/// distinct from the marker-file scenario above: cancelling
/// `execute()` used to hang well past the 5s grace period (observed:
/// did not return within 20s) because only the immediate `sh` process was
/// signalled. Its trap made it immune to SIGTERM, and after the grace
/// period the single `child.kill()` call only reached that one process —
/// never the backgrounded `sleep 30` still holding the output pipe open —
/// so the reader-task await after that point blocked forever too. With
/// both SIGTERM and the escalated SIGKILL now targeting the whole process
/// group, the backgrounded `sleep 30` (which does not itself trap
/// anything) dies from the group-wide SIGTERM almost immediately, `wait`
/// unblocks, and the shell exits well under a second later — nowhere near
/// even the 5s grace period, let alone 20s.
#[cfg(unix)]
#[tokio::test]
async fn cancellation_of_a_trap_ignoring_command_with_a_backgrounded_child_does_not_hang() {
    let (context, _rx) = ctx();
    let cancel = context.cancel.clone();
    let cancelled_at = cancel_after(cancel, Duration::from_millis(200));

    let _result = SystemShellExecutor
        .execute(spec("trap '' TERM; sleep 30 & wait"), context)
        .await
        .unwrap();
    let elapsed = tokio::time::Instant::now() - cancelled_at.await.unwrap();

    assert!(
        elapsed < Duration::from_secs(7),
        "execute() took {elapsed:?} to return; this used to hang past 20s"
    );
}

/// An uncancelled run that merely backgrounds a descendant (`sleep 6 &`)
/// must not block `execute()` until that descendant finishes: the shell
/// itself exits almost immediately, but without a bound on how long the
/// reader tasks wait for EOF on the inherited pipes, `execute()` would
/// hang until the backgrounded process's pipe references closed six
/// seconds later. `DRAIN_PERIOD` bounds that wait.
#[cfg(unix)]
#[tokio::test]
async fn uncancelled_run_does_not_block_on_a_backgrounded_descendant() {
    let (context, _rx) = ctx();
    let start = tokio::time::Instant::now();
    let result = SystemShellExecutor
        .execute(spec("sleep 6 & echo done"), context)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(result.exit_code, 0);
    assert!(
        elapsed < Duration::from_secs(3),
        "execute() took {elapsed:?}; a backgrounded descendant should not \
         block the return (DRAIN_PERIOD bounds this to ~2s)"
    );
}
