//! Integration tests for [`alba_executors::EmbeddedShellExecutor`].
//!
//! Mirrors `tests/shell.rs`'s harness style, but for the embedded shell:
//! no external process is spawned for the shell itself, so the same
//! command runs identically on every platform. `cargo --version` is the
//! one case here that does spawn an external program, to prove the
//! process environment (in particular `PATH`) reaches it.

use alba_executors::{CommandSpec, EmbeddedShellExecutor, ExecContext, Executor, Stream};
use tokio_util::sync::CancellationToken;

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
    let result = EmbeddedShellExecutor
        .execute(spec, ctx)
        .await
        .map(|r| r.exit_code)
        .map_err(|e| e.to_string());
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
