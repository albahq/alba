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

#[tokio::test]
async fn a_pipeline_connects_stdout_to_stdin() {
    // cat (task 6) is not here yet, so pipe external into external is the
    // only option; keep it minimal: exit code of the last stage wins.
    let (code, _) = run("cargo --version | cargo definitely-not-a-subcommand").await;
    assert_ne!(code, 0, "last stage decides");
    let (code, _) = run("cargo definitely-not-a-subcommand | cargo --version").await;
    assert_eq!(code, 0, "last stage decides");
}

#[tokio::test]
async fn stdout_redirects_to_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("pwd > out.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(
        stdout(&lines).is_empty(),
        "redirected output must not reach the channel"
    );
    let contents = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert!(
        contents.trim_end().ends_with(
            &dir.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        )
    );
}

#[tokio::test]
async fn append_redirect_appends() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _) = run_in("pwd > out.txt; pwd >> out.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    let contents = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert_eq!(contents.lines().count(), 2);
}

#[tokio::test]
async fn stderr_redirects_independently() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cd nowhere 2> err.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.is_empty(), "stderr went to the file");
    let contents = std::fs::read_to_string(dir.path().join("err.txt")).unwrap();
    assert!(contents.contains("nowhere"));
}

#[tokio::test]
async fn stderr_to_stdout_retags_lines() {
    let (_, lines) = run("cd definitely-nowhere 2>&1").await;
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stdout && t.contains("definitely-nowhere"))
    );
}

#[tokio::test]
async fn an_unopenable_redirect_fails_without_running_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("pwd > missing_dir/out.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t.contains("cannot open"))
    );
}

#[tokio::test]
async fn pipeline_stages_do_not_leak_state() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("cd sub | true; pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(
        !stdout(&lines).join("").ends_with("sub"),
        "cd inside a pipeline must not move the shell"
    );
}

#[tokio::test]
async fn exit_inside_a_pipeline_does_not_stop_the_program() {
    let (code, _) = run("exit 5 | true; true").await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn input_redirect_feeds_an_external() {
    // Full stdin coverage arrives with `cat` in Task 6; here only assert
    // that `< file` on a missing file fails cleanly.
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("true < missing.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(_, t)| t.contains("cannot open")));
}

// The four tests below cover what the rest of this file cannot reach
// with builtins alone: stages that genuinely run at the same time, and
// pipe ends left open in this process — a fault that shows up not as a
// wrong value but as a run that never returns. Each needs a real
// producer, consumer or long-running process, which no builtin can
// supply until `cat` and `sleep` arrive; `sh` is the only portable
// stand-in, so they are unix-only, like the other `#[cfg(unix)]` tests
// in this crate that need a real executable to exist.

#[cfg(unix)]
#[tokio::test]
async fn cancellation_stops_every_stage_of_a_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    // Each stage leaves a marker the instant it starts, then sleeps far
    // longer than the test can tolerate. Both markers prove both stages
    // really started; the prompt return proves both were terminated,
    // since `execute` cannot return until every stage has been joined.
    let source = format!(
        "sh -c 'touch {0}/first; sleep 30' | sh -c 'touch {0}/second; sleep 30'",
        dir.path().display()
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse(&source).unwrap();
    let cancel = CancellationToken::new();
    let env = ShellEnv {
        env: std::env::vars().collect(),
        cwd: std::env::current_dir().unwrap(),
        output: tx,
        cancel: cancel.clone(),
    };

    let handle = tokio::spawn(async move { execute(&program, env).await });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let cancelled_at = std::time::Instant::now();
    cancel.cancel();

    let result = handle.await.expect("execute task panicked");
    let elapsed = cancelled_at.elapsed();

    assert!(dir.path().join("first").exists(), "the first stage ran");
    assert!(dir.path().join("second").exists(), "the second stage ran");
    assert_eq!(result.exit_code, 130);
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "execute() took {elapsed:?} to return after cancellation; expected \
         the graceful-stop signal to kill both sleeping stages almost \
         immediately, not the 5s grace period or the full 30s sleep"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_producer_larger_than_the_pipe_buffer_is_neither_blocked_nor_truncated() {
    // 200_000 lines is far past any platform's pipe buffer, so the first
    // stage cannot finish until the second drains it: this only returns
    // if both stages really are running at once, and only counts right
    // if nothing was dropped between them.
    let (code, lines) = run("sh -c 'seq 1 200000' | sh -c 'wc -l'").await;
    assert_eq!(code, 0, "lines: {lines:?}");
    assert_eq!(stdout(&lines).join("").trim(), "200000");
}

#[cfg(unix)]
#[tokio::test]
async fn a_stage_that_never_reads_does_not_hang_its_producer() {
    // `true` ignores its input, so the read end of the pipe feeding it
    // must be closed rather than held: otherwise the producer blocks
    // forever on a pipe nobody will ever drain.
    let (code, lines) = run("sh -c 'seq 1 200000' | true").await;
    assert_eq!(code, 0, "lines: {lines:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_stage_that_fails_before_running_still_closes_its_pipe() {
    // The first stage never runs (its redirect cannot be opened), which
    // is exactly the error path where a stage is likeliest to leak the
    // write end it was handed and leave the next stage waiting on an
    // EOF that never comes.
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("pwd > nope/out.txt | sh -c 'cat'", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "the last stage decides, lines: {lines:?}");
    assert!(
        lines.iter().any(|(_, t)| t.contains("cannot open")),
        "lines: {lines:?}"
    );
}
