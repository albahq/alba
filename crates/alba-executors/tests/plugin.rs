//! Integration tests for `PluginExecutor`, driven against the scripted
//! fake plugin binary in `tests/support/fake_plugin.rs`.

use std::path::PathBuf;
use std::time::Duration;

use alba_executors::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecSession, Executor, OutputLine,
    PluginExecutor, Stream,
};
use tokio_util::sync::CancellationToken;

/// `Box<dyn ExecSession>` is not `Debug`, so `Result::expect_err` cannot be
/// used directly on `open`'s return type; this does the same job by hand.
fn expect_open_err(result: Result<Box<dyn ExecSession>, ExecError>, msg: &str) -> ExecError {
    match result {
        Ok(_session) => panic!("{msg}"),
        Err(error) => error,
    }
}

fn fake_plugin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fake-plugin"))
}

fn executor_for(mode: &str) -> PluginExecutor {
    PluginExecutor::new(fake_plugin_path()).with_args(vec![mode.to_string()])
}

fn beam_context(output: tokio::sync::mpsc::UnboundedSender<OutputLine>) -> BeamContext {
    BeamContext {
        beam: "test".to_string(),
        dir: std::env::temp_dir(),
        options: serde_json::Value::Null,
        output,
        cancel: CancellationToken::new(),
    }
}

fn exec_context(
    output: tokio::sync::mpsc::UnboundedSender<OutputLine>,
    cancel: CancellationToken,
) -> ExecContext {
    ExecContext { output, cancel }
}

fn command(text: &str) -> CommandSpec {
    CommandSpec {
        command: text.to_string(),
        env: vec![],
        cwd: std::env::temp_dir(),
    }
}

/// Bound used to keep a regression that made some `await` hang forever
/// from turning into an indefinite CI hang; well above anything these
/// tests are meant to take, so it never fires on correct behavior.
const HANG_GUARD: Duration = Duration::from_secs(3);

#[tokio::test]
async fn handshake_execute_and_close_roundtrip() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("ok");
    let mut session = tokio::time::timeout(HANG_GUARD, executor.open(beam_context(tx.clone())))
        .await
        .expect("open() must not hang")
        .expect("handshake with a well-behaved plugin must succeed");

    let result = tokio::time::timeout(
        HANG_GUARD,
        session.execute(
            command("echo ping"),
            exec_context(tx.clone(), CancellationToken::new()),
        ),
    )
    .await
    .expect("execute() must not hang")
    .expect("echo must succeed");
    assert_eq!(result.exit_code, 0);
    let line = tokio::time::timeout(HANG_GUARD, rx.recv())
        .await
        .expect("the echoed line must arrive before the guard elapses")
        .expect("the channel must not close before the line arrives");
    assert_eq!(
        line,
        OutputLine {
            stream: Stream::Stdout,
            text: "ping".to_string(),
        }
    );

    let result = tokio::time::timeout(
        HANG_GUARD,
        session.execute(
            command("fail 3"),
            exec_context(tx.clone(), CancellationToken::new()),
        ),
    )
    .await
    .expect("execute() must not hang")
    .expect("a plugin-reported exit is not an executor error");
    assert_eq!(result.exit_code, 3);

    tokio::time::timeout(HANG_GUARD, session.close())
        .await
        .expect("close() must not hang")
        .expect("close must succeed");
}

#[tokio::test]
async fn plugin_stderr_lands_in_the_beams_output_as_stderr() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("ok");
    let mut session = executor
        .open(beam_context(tx.clone()))
        .await
        .expect("handshake with a well-behaved plugin must succeed");

    let result = session
        .execute(
            command("stderr-probe"),
            exec_context(tx.clone(), CancellationToken::new()),
        )
        .await
        .expect("stderr-probe must succeed");
    assert_eq!(result.exit_code, 0);

    // The stderr line is relayed by a task independent from the protocol
    // messages on stdout, so it can arrive before or after `execute`
    // returns; poll for it instead of assuming an order. The exact text
    // is asserted, not just the stream, so this cannot pass on an
    // unrelated future stderr line.
    let mut stderr_text = None;
    while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        if line.stream == Stream::Stderr {
            stderr_text = Some(line.text);
            break;
        }
    }
    assert_eq!(
        stderr_text.as_deref(),
        Some("raw stderr line"),
        "expected the plugin's exact stderr line"
    );

    session.close().await.expect("close must succeed");
}

#[tokio::test]
async fn a_mute_plugin_fails_the_handshake_within_the_timeout() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = PluginExecutor::with_timeouts(
        fake_plugin_path(),
        Duration::from_millis(500),
        Duration::from_secs(5),
    )
    .with_args(vec!["mute".to_string()]);

    // Bounding the whole call: if the handshake timeout fired but the
    // child was never killed, `open` would hang forever waiting to reap
    // it instead of returning promptly.
    let result = tokio::time::timeout(Duration::from_secs(3), executor.open(beam_context(tx)))
        .await
        .expect("open() must not hang past the handshake timeout");
    let error = expect_open_err(result, "a mute plugin must fail the handshake");
    assert!(error.to_string().contains("handshake"), "{error}");
}

#[tokio::test]
async fn a_garbage_line_is_a_protocol_error_quoting_the_line() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("garbage");

    let result = tokio::time::timeout(Duration::from_secs(3), executor.open(beam_context(tx)))
        .await
        .expect("open() must not hang on a garbage line");
    let error = expect_open_err(result, "a garbage line must fail the handshake");
    assert!(error.to_string().contains("this is not json"), "{error}");
}

#[tokio::test]
async fn a_refusing_plugin_surfaces_its_own_error_message() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("refuse");

    let result = tokio::time::timeout(Duration::from_secs(3), executor.open(beam_context(tx)))
        .await
        .expect("open() must not hang on a refusing plugin");
    let error = expect_open_err(result, "a refusing plugin must fail the handshake");
    assert!(
        error.to_string().contains("protocol 1 not supported"),
        "{error}"
    );
}

#[tokio::test]
async fn a_dead_plugin_reports_its_exit_code() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("die");

    let result = tokio::time::timeout(Duration::from_secs(3), executor.open(beam_context(tx)))
        .await
        .expect("open() must not hang on a plugin that dies immediately");
    let error = expect_open_err(result, "a dead plugin must fail the handshake");
    let message = error.to_string();
    assert!(message.contains("exited"), "{message}");
    assert!(message.contains("code 3"), "{message}");
}

#[tokio::test]
async fn cancel_asks_first_then_kills_after_the_grace() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    // The grace is set much wider than what a correct run needs (a
    // plugin answering `cancel` should return in roughly the 50ms until
    // cancellation fires, not anywhere near the grace) so the margin
    // between "correct" and "grace fired instead" is wide: correct lands
    // at ~55ms, incorrect at ~2s. A tight grace (e.g. 300ms) leaves the
    // machine only a couple hundred milliseconds of slack to fire a
    // timer and complete a two-process round trip while the rest of the
    // suite runs concurrently, which is a real source of flakiness under
    // load, not just theoretical.
    let grace = Duration::from_secs(2);
    let executor =
        PluginExecutor::with_timeouts(fake_plugin_path(), Duration::from_secs(10), grace)
            .with_args(vec!["ok".to_string()]);
    let mut session = executor
        .open(beam_context(tx.clone()))
        .await
        .expect("handshake with a well-behaved plugin must succeed");

    let cancel = CancellationToken::new();
    let cancel_after_50ms = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_after_50ms.cancel();
    });

    let started = tokio::time::Instant::now();
    let result = session
        .execute(command("hang"), exec_context(tx.clone(), cancel))
        .await
        .expect("a plugin that answers cancel is not an executor error");
    let elapsed = started.elapsed();

    assert_eq!(result.exit_code, -1);
    assert!(
        elapsed < Duration::from_millis(500),
        "expected to return well before the {grace:?} grace, took {elapsed:?}"
    );

    session.close().await.expect("close must succeed");
}

#[tokio::test]
async fn a_cancel_deaf_plugin_is_killed_after_the_grace() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let grace = Duration::from_millis(300);
    let executor =
        PluginExecutor::with_timeouts(fake_plugin_path(), Duration::from_secs(10), grace)
            .with_args(vec!["deaf".to_string()]);
    let mut session = executor
        .open(beam_context(tx.clone()))
        .await
        .expect("a deaf plugin still answers the handshake");

    let cancel = CancellationToken::new();
    let cancel_after_50ms = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_after_50ms.cancel();
    });

    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        session.execute(command("hang"), exec_context(tx.clone(), cancel)),
    )
    .await
    .expect("execute() must not hang past the grace period")
    .expect("a killed-after-grace command is not an executor error");
    let elapsed = started.elapsed();

    assert_eq!(result.exit_code, -1);
    assert!(
        elapsed >= Duration::from_millis(50),
        "cancel fired at 50ms, returned after only {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "expected roughly the {grace:?} grace, took {elapsed:?}"
    );

    session.close().await.expect("close must succeed");
}

#[tokio::test]
async fn a_plugin_that_never_reads_stdin_does_not_hang_the_handshake_write() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = PluginExecutor::with_timeouts(
        fake_plugin_path(),
        Duration::from_millis(300),
        Duration::from_secs(5),
    )
    .with_args(vec!["stubborn".to_string()]);

    let mut beam = beam_context(tx);
    // Large enough to exceed any platform's default stdin pipe buffer
    // (typically tens of KiB), so the `open` message write blocks once
    // it fills — `stubborn` never reads a single byte, so nothing ever
    // drains it. Before bounding the write itself (not just the read
    // that follows it), this would hang past the handshake timeout.
    beam.options = serde_json::json!({ "padding": "x".repeat(4 * 1024 * 1024) });

    let result = tokio::time::timeout(HANG_GUARD, executor.open(beam))
        .await
        .expect("open() must not hang past the handshake timeout even while still writing");
    let error = expect_open_err(
        result,
        "a plugin that never reads stdin must fail the handshake",
    );
    assert!(error.to_string().contains("handshake"), "{error}");
}

#[tokio::test]
async fn a_cancelled_beam_interrupts_a_pending_handshake() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = PluginExecutor::with_timeouts(
        fake_plugin_path(),
        Duration::from_secs(10),
        Duration::from_secs(5),
    )
    .with_args(vec!["mute".to_string()]);

    let mut beam = beam_context(tx);
    let cancel = CancellationToken::new();
    beam.cancel = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();
    });

    let started = tokio::time::Instant::now();
    let result = tokio::time::timeout(HANG_GUARD, executor.open(beam))
        .await
        .expect("open() must not hang past the caller's cancellation");
    let elapsed = started.elapsed();
    expect_open_err(
        result,
        "a cancelled beam must interrupt a pending handshake",
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "expected to return shortly after the 50ms cancellation, took {elapsed:?} \
         (the handshake timeout is 10s, so a value near that means cancellation was ignored)"
    );
}

/// Unix-only: shells out to `ps` to look for a still-running child of this
/// test process, which windows has no equivalent one-liner for. The other
/// tests already cover kill-and-reap on every path that runs explicit
/// cleanup (a failed handshake, an expired grace, `close`); this is the
/// one path that doesn't — dropping the session outright — so it is the
/// one test that needs to look at the OS process table instead of at
/// `PluginExecutor`'s return values.
#[cfg(unix)]
#[tokio::test]
async fn a_dropped_session_does_not_leak_the_plugin_process() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("deaf");
    let session = tokio::time::timeout(HANG_GUARD, executor.open(beam_context(tx)))
        .await
        .expect("open() must not hang")
        .expect("a deaf plugin still answers the handshake");

    // Dropping the session without calling `close()` — a panic, an
    // `execute` error the caller never recovers from, a cancelled task
    // holding the session — must not leave the plugin running forever.
    // `kill_on_drop` is the backstop for exactly this path; every other
    // path already kills and waits explicitly.
    let my_pid = std::process::id().to_string();
    drop(session);

    let mut still_running = true;
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let ps = std::process::Command::new("ps")
            .args(["-eo", "pid,ppid,command"])
            .output()
            .expect("ps must run");
        let text = String::from_utf8_lossy(&ps.stdout);
        still_running = text.lines().any(|line| {
            // Matched by field, not by a raw substring search over the
            // whole line: a substring search also matches unrelated
            // processes whose command line merely mentions
            // "fake-plugin" in passing (this very test's own source, if
            // it were running under a shell that echoes it, for one).
            let fields: Vec<&str> = line.split_whitespace().collect();
            fields.len() >= 3 && fields[1] == my_pid && fields[2].ends_with("fake-plugin")
        });
        if !still_running {
            break;
        }
    }
    assert!(
        !still_running,
        "the dropped plugin process is still running as our child 3s later"
    );
}
