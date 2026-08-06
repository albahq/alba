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

#[tokio::test]
async fn handshake_execute_and_close_roundtrip() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = executor_for("ok");
    let mut session = executor
        .open(beam_context(tx.clone()))
        .await
        .expect("handshake with a well-behaved plugin must succeed");

    let result = session
        .execute(
            command("echo ping"),
            exec_context(tx.clone(), CancellationToken::new()),
        )
        .await
        .expect("echo must succeed");
    assert_eq!(result.exit_code, 0);
    let line = rx.recv().await.expect("the echoed line must arrive");
    assert_eq!(
        line,
        OutputLine {
            stream: Stream::Stdout,
            text: "ping".to_string(),
        }
    );

    let result = session
        .execute(
            command("fail 3"),
            exec_context(tx.clone(), CancellationToken::new()),
        )
        .await
        .expect("a plugin-reported exit is not an executor error");
    assert_eq!(result.exit_code, 3);

    session.close().await.expect("close must succeed");
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
    // returns; poll for it instead of assuming an order.
    let mut saw_stderr = false;
    while let Ok(Some(line)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        if line.stream == Stream::Stderr {
            saw_stderr = true;
            break;
        }
    }
    assert!(saw_stderr, "expected a stderr OutputLine from the plugin");

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
    assert!(message.contains('3'), "{message}");
}

#[tokio::test]
async fn cancel_asks_first_then_kills_after_the_grace() {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let executor = PluginExecutor::with_timeouts(
        fake_plugin_path(),
        Duration::from_secs(10),
        Duration::from_millis(300),
    )
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
        elapsed < Duration::from_millis(300),
        "expected to return well before the 300ms grace, took {elapsed:?}"
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
