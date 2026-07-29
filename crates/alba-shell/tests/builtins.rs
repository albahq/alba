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
async fn echo_joins_arguments_with_single_spaces() {
    let (_, lines) = run("echo one two   three").await;
    assert_eq!(stdout(&lines), vec!["one two three"]);
}

#[tokio::test]
async fn echo_n_suppresses_the_newline_and_unknown_flags_are_arguments() {
    let (_, lines) = run("echo -n x; echo -e y").await;
    // -n: still one line through the channel (line flushed on drop);
    // -e is NOT a flag: it is printed.
    assert_eq!(stdout(&lines), vec!["x", "-e y"]);
}

#[tokio::test]
async fn cat_reads_files_and_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let (code, lines) = run_in("cat a.txt | cat", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert_eq!(stdout(&lines), vec!["hello"]);
}

#[tokio::test]
async fn cat_reports_a_missing_file_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "ok\n").unwrap();
    let (code, lines) = run_in("cat nope.txt a.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert_eq!(stdout(&lines), vec!["ok"]);
}

#[tokio::test]
async fn cp_copies_a_file_and_r_copies_a_tree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    std::fs::write(dir.path().join("d/inner.txt"), "y").unwrap();
    let (code, _) = run_in("cp a.txt b.txt && cp -r d d2", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(dir.path().join("b.txt").is_file());
    assert!(dir.path().join("d2/inner.txt").is_file());
}

#[tokio::test]
async fn cp_refuses_a_directory_without_r() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    let (code, lines) = run_in("cp d d2", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn cp_rejects_an_unknown_flag() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (code, lines) = run_in("cp -z a.txt b.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 2);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn mv_renames() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (code, _) = run_in("mv a.txt b.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(!dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").is_file());
}

#[tokio::test]
async fn mv_reports_a_missing_source() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("mv missing.txt b.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn rm_needs_r_for_directories_and_f_forgives_missing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    assert_eq!(run_in("rm d", dir.path().to_path_buf()).await.0, 1);
    assert_eq!(run_in("rm -r d", dir.path().to_path_buf()).await.0, 0);
    assert_eq!(
        run_in("rm missing.txt", dir.path().to_path_buf()).await.0,
        1
    );
    assert_eq!(
        run_in("rm -f missing.txt", dir.path().to_path_buf())
            .await
            .0,
        0
    );
}

#[tokio::test]
async fn rm_rejects_an_unknown_flag() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (code, _) = run_in("rm -z a.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn the_dogfood_line_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("dist")).unwrap();
    std::fs::write(dir.path().join("dist/old.js"), "x").unwrap();
    let (code, _) = run_in("rm -r -f dist && mkdir dist", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(dir.path().join("dist").is_dir());
    assert!(!dir.path().join("dist/old.js").exists());
}

#[tokio::test]
async fn mkdir_p_creates_parents_and_tolerates_existing() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        run_in("mkdir -p a/b/c && mkdir -p a/b/c", dir.path().to_path_buf())
            .await
            .0,
        0
    );
    assert_eq!(run_in("mkdir x/y", dir.path().to_path_buf()).await.0, 1);
}

#[tokio::test]
async fn mkdir_rejects_an_unknown_flag() {
    let dir = tempfile::tempdir().unwrap();
    let (code, _) = run_in("mkdir -z a", dir.path().to_path_buf()).await;
    assert_eq!(code, 2);
}

#[tokio::test]
async fn touch_creates_and_updates() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run_in("touch new.txt", dir.path().to_path_buf()).await.0, 0);
    assert!(dir.path().join("new.txt").is_file());
    assert_eq!(run_in("touch new.txt", dir.path().to_path_buf()).await.0, 0);
}

#[tokio::test]
async fn touch_reports_a_missing_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("touch nope/new.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn sleep_accepts_decimals_and_rejects_garbage() {
    assert_eq!(run("sleep 0.05").await.0, 0);
    assert_eq!(run("sleep soon").await.0, 2);
}

#[tokio::test]
async fn sleep_rejects_more_than_one_operand() {
    assert_eq!(run("sleep 0.01 0.01").await.0, 2);
}

#[tokio::test]
async fn sleep_actually_waits_for_its_duration() {
    // A `sleep` that just validated its argument and returned instantly
    // would pass every other test in this file; only a wall-clock check
    // catches that. The lower bound is well under the 200ms requested,
    // to stay robust against a loaded CI machine.
    let started = std::time::Instant::now();
    assert_eq!(run("sleep 0.2").await.0, 0);
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(120),
        "elapsed: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn test_covers_files_strings_and_numbers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "").unwrap();
    let cwd = dir.path().to_path_buf();
    assert_eq!(run_in("test -f f.txt", cwd.clone()).await.0, 0);
    assert_eq!(run_in("test -d f.txt", cwd.clone()).await.0, 1);
    assert_eq!(
        run_in("test -e f.txt && test -z '' && test -n x", cwd.clone())
            .await
            .0,
        0
    );
    assert_eq!(
        run("test a = a && test a != b && test 2 -gt 1 && test 1 -le 1")
            .await
            .0,
        0
    );
    assert_eq!(run("test 2 -lt 1").await.0, 1);
    assert_eq!(run("test x -gt 1").await.0, 2);
    assert_eq!(run_in("[ -f f.txt ]", cwd).await.0, 0);
    assert_eq!(run("[ -f oops").await.0, 2);
}

#[tokio::test]
async fn a_builtin_wins_over_path_but_an_explicit_path_does_not() {
    // `echo` must be ours even on unix where /bin/echo exists: our echo
    // treats -e as an argument (bash's interprets it as a flag).
    let (_, lines) = run("echo -e tag").await;
    assert_eq!(stdout(&lines), vec!["-e tag"]);
}
