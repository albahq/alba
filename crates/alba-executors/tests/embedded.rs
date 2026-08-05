//! Integration tests for [`alba_executors::EmbeddedShellExecutor`].
//!
//! Mirrors `tests/shell.rs`'s harness style, but for the embedded shell:
//! no external process is spawned for the shell itself, so the same
//! command runs identically on every platform. `cargo --version` is the
//! one case here that does spawn an external program, to prove the
//! process environment (in particular `PATH`) reaches it.

use alba_executors::{
    BeamContext, CommandSpec, EmbeddedShellExecutor, ExecContext, Executor, Stream,
};
use tokio_util::sync::CancellationToken;

async fn open_session() -> Box<dyn alba_executors::ExecSession> {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let context = BeamContext {
        beam: "test".to_string(),
        dir: std::env::current_dir().unwrap(),
        options: serde_json::Value::Null,
        output: tx,
        cancel: CancellationToken::new(),
    };
    EmbeddedShellExecutor.open(context).await.unwrap()
}

async fn exec(
    command: &str,
    env: Vec<(String, String)>,
) -> (Result<i32, String>, Vec<(Stream, String)>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let spec = CommandSpec {
        command: command.to_string(),
        env,
        cwd: std::env::current_dir().unwrap(),
    };
    let ctx = ExecContext {
        output: tx,
        cancel: CancellationToken::new(),
    };
    let mut session = open_session().await;
    let result = session
        .execute(spec, ctx)
        .await
        .map(|r| r.exit_code)
        .map_err(|e| e.to_string());
    session.close().await.unwrap();
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push((line.stream, line.text));
    }
    (result, lines)
}

#[tokio::test]
async fn runs_a_builtin_identically_everywhere() {
    let (result, lines) = exec("echo -n one && echo two", vec![]).await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(
        lines,
        vec![
            (Stream::Stdout, "one".into()),
            (Stream::Stdout, "two".into())
        ]
    );
}

#[tokio::test]
async fn beam_env_overlays_the_process_environment() {
    let (result, lines) = exec(
        "echo $ALBA_EMBEDDED_TEST",
        vec![("ALBA_EMBEDDED_TEST".into(), "on".into())],
    )
    .await;
    assert_eq!(result.unwrap(), 0);
    assert_eq!(lines, vec![(Stream::Stdout, "on".into())]);
}

#[tokio::test]
async fn path_from_the_process_environment_reaches_externals() {
    let (result, _) = exec("cargo --version", vec![]).await;
    assert_eq!(result.unwrap(), 0);
}

#[tokio::test]
async fn a_parse_error_is_an_exec_error_carrying_the_diagnostic() {
    let (result, lines) = exec("for x in a; do echo $x; done", vec![]).await;
    let message = result.unwrap_err();
    assert!(message.contains("`for` loops are not supported"));
    assert!(message.contains("executor system_shell"));
    assert!(message.contains('^'), "the rendered span must be present");
    assert!(lines.is_empty(), "nothing may have run");
}

// The cancellation contract at the level the engine actually consumes:
// `ExecSession::execute` must return promptly once the token fires, awaiting
// exactly what a real caller awaits.

use std::time::{Duration, Instant};

/// Turns this test binary into the external command the cancellation test
/// spawns. Unset (the ordinary `cargo test` run), the helper does nothing.
const HELPER_ROLE: &str = "ALBA_EMBEDDED_HELPER_ROLE";

/// How long the escaped descendant keeps the inherited pipe open.
const DESCENDANT_LIFETIME: Duration = Duration::from_secs(5);

/// Not an assertion: this test doubles as the external command
/// [`cancellation_returns_promptly_when_a_stage_outlives_the_run`] runs.
/// As `parent` it starts a descendant of its own that inherits its stdout
/// (which is the shell's pipe) and then exits at once, leaving that
/// descendant holding the write end. As `descendant` it is that process,
/// and simply holds the handle for a while.
///
/// A nested pair of self-invocations rather than `sh -c 'sleep 5 & echo
/// hi'` precisely so the case is not unix-only: what it reproduces is a
/// stage the shell can no longer reach, which every platform can produce.
// Not waiting on the descendant is the whole point: the parent has to
// exit while it is still running. It is reparented and reaped by the
// system the moment the parent goes, so no zombie survives.
#[allow(clippy::zombie_processes)]
#[test]
fn escaping_descendant_helper() {
    match std::env::var(HELPER_ROLE).as_deref() {
        Ok("parent") => {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["escaping_descendant_helper", "--exact"])
                .env(HELPER_ROLE, "descendant")
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawning the descendant");
        }
        Ok("descendant") => std::thread::sleep(DESCENDANT_LIFETIME),
        _ => {}
    }
}

/// Runs `<helper> | cat`, cancels it half a second in, and returns how
/// long `ExecSession::execute` then took to come back. The producer exits
/// at once but its descendant keeps the pipe open, so the `cat` stage stays
/// parked in a blocking read the shell can neither abort nor wait out.
async fn cancel_a_run_a_stage_outlives() -> Duration {
    let helper = std::env::current_exe().unwrap();
    let spec = CommandSpec {
        command: format!(
            "'{}' escaping_descendant_helper --exact | cat",
            helper.display()
        ),
        env: vec![(HELPER_ROLE.to_string(), "parent".to_string())],
        cwd: std::env::current_dir().unwrap(),
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let ctx = ExecContext {
        output: tx,
        cancel: cancel.clone(),
    };

    let mut session = open_session().await;
    let handle = tokio::spawn(async move { session.execute(spec, ctx).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let cancelled_at = Instant::now();
    cancel.cancel();

    let result = tokio::time::timeout(Duration::from_secs(60), handle)
        .await
        .expect("execute() never returned after cancellation")
        .expect("execute task panicked");
    let elapsed = cancelled_at.elapsed();
    assert_eq!(result.unwrap().exit_code, 130);
    elapsed
}

#[tokio::test]
async fn cancellation_returns_promptly_when_a_stage_outlives_the_run() {
    // `execute` must return on the token, which means it must not be
    // waiting on anything that outlives the run.
    let elapsed = cancel_a_run_a_stage_outlives().await;
    assert!(
        elapsed < Duration::from_secs(2),
        "execute() took {elapsed:?} to return after cancellation; expected \
         it to stop waiting on a stage the run no longer owns, not to sit \
         out the descendant still holding the pipe"
    );
}

#[test]
fn a_cancelled_run_does_not_pin_the_host_process() {
    // The other half of the same contract, and the half a user actually
    // feels: returning promptly buys nothing if the process then cannot
    // exit. A tokio runtime waits for every blocking-pool task before it
    // can be dropped, so an abandoned stage parked there holds the whole
    // program open for as long as whatever it is reading stays open: the
    // summary prints the instant Ctrl-C lands, and the shell prompt comes
    // back seconds later.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(cancel_a_run_a_stage_outlives());

    let started = Instant::now();
    drop(runtime);
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "dropping the runtime took {elapsed:?} after a cancelled run; the \
         abandoned stage must not be something the runtime has to wait for"
    );
}
