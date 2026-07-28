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
