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
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t == "cat: nope.txt: no such file"),
        "lines: {lines:?}"
    );
}

#[tokio::test]
async fn cat_rejects_an_unknown_flag() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cat -n missing.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 2, "lines: {lines:?}");
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
}

#[tokio::test]
async fn cat_reports_a_directory_argument() {
    // `File::open` on a directory succeeds on unix; the failure only
    // shows up once `cat` tries to read from it. Before the fix this
    // silently exited 1 with no stderr at all.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("cat sub", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t.contains("sub")),
        "lines: {lines:?}"
    );
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
async fn mv_rejects_an_unknown_flag() {
    // `mv` has no flags at all: a leading `-r` must not be read as a
    // (nonexistent) source file named `-r`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let (code, lines) = run_in("mv -r a.txt b.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 2, "lines: {lines:?}");
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
async fn touch_rejects_an_unknown_flag() {
    // `touch` has no flags at all: `-x` must not be read as a file name.
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("touch -x", dir.path().to_path_buf()).await;
    assert_eq!(code, 2, "lines: {lines:?}");
    assert!(!dir.path().join("-x").exists());
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
async fn sleep_rejects_a_dash_argument() {
    let (code, lines) = run("sleep -x").await;
    assert_eq!(code, 2, "lines: {lines:?}");
    assert!(lines.iter().any(|(s, _)| *s == ShellStream::Stderr));
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
async fn test_covers_the_remaining_numeric_operators() {
    // `test_covers_files_strings_and_numbers` above only exercises
    // `-gt`, `-le`, and `-lt`; this closes the other half of `binary`'s
    // match (`-eq`, `-ne`, `-ge`).
    assert_eq!(
        run("test 1 -eq 1 && test 1 -ne 2 && test 2 -ge 2").await.0,
        0
    );
    assert_eq!(run("test 1 -eq 2").await.0, 1);
    assert_eq!(run("test 1 -ne 1").await.0, 1);
    assert_eq!(run("test 1 -ge 2").await.0, 1);
}

#[tokio::test]
async fn a_builtin_wins_over_path_but_an_explicit_path_does_not() {
    // `echo` must be ours even on unix where /bin/echo exists: our echo
    // treats -e as an argument (bash's interprets it as a flag).
    //
    // Note: this discriminates on Linux, where GNU coreutils' `/bin/echo`
    // really does treat a leading `-e` as a flag (enabling backslash
    // escapes) and would print just `tag`. It does *not* discriminate on
    // macOS, where BSD's `/bin/echo` has no `-e` either and would
    // coincidentally print `-e tag` too — see the two tests below for a
    // check that holds on every platform regardless of which `echo`
    // happens to be installed.
    let (_, lines) = run("echo -e tag").await;
    assert_eq!(stdout(&lines), vec!["-e tag"]);
}

#[tokio::test]
async fn a_builtin_wins_even_with_no_path_binary_of_the_same_name() {
    // Point `PATH` at a directory that provably has no `echo` in it (or
    // anything else): if the builtin lookup were ever skipped, the
    // subsequent `PATH` search would fail outright and this would exit
    // 127 ("command not found"), never 0. This holds regardless of which
    // real `echo` (if any) happens to be installed, unlike the test
    // above.
    let empty_path = tempfile::tempdir().unwrap();
    let env = vec![("PATH".to_string(), empty_path.path().display().to_string())];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let program = parse("echo hi").expect("parse");
    let result = execute(
        &program,
        ShellEnv {
            env,
            cwd: std::env::current_dir().unwrap(),
            output: tx,
            cancel: CancellationToken::new(),
        },
    )
    .await;
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push((line.stream, line.text));
    }
    assert_eq!(
        result.exit_code, 0,
        "an external lookup on an echo-less PATH would have failed with 127; \
         only the builtin can have produced 0, lines: {lines:?}"
    );
    assert_eq!(stdout(&lines), vec!["hi"]);
}

#[tokio::test]
async fn an_explicit_path_bypasses_the_builtin_lookup_entirely() {
    // `./true` (a path, not a bare name) must never fall back to the
    // `true` builtin just because no such file exists at that path: an
    // explicit path bypasses builtin dispatch outright, so this is
    // "command not found" (127), not the builtin's exit 0. This is the
    // "an explicit path does not win" half the test above only names,
    // never exercises.
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("./true", dir.path().to_path_buf()).await;
    assert_eq!(code, 127, "lines: {lines:?}");
}

#[tokio::test]
async fn grouped_short_flags_are_decomposed() {
    // `rm -rf` is the spelling every real script uses, and it must mean
    // exactly `rm -r -f`; likewise `mkdir -pv` would name `-p` and `-v`
    // separately rather than one unknown option called `-pv`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("dist/nested")).unwrap();
    std::fs::write(dir.path().join("dist/nested/old.js"), "x").unwrap();
    let (code, lines) = run_in("rm -rf dist && mkdir dist", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
    assert!(dir.path().join("dist").is_dir());
    assert!(!dir.path().join("dist/nested").exists());
}

#[tokio::test]
async fn a_grouped_flag_reports_the_single_letter_it_did_not_know() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("rm -rz a.txt", dir.path().to_path_buf()).await;
    assert_eq!(code, 2, "lines: {lines:?}");
    assert!(
        lines.iter().any(
            |(stream, text)| *stream == ShellStream::Stderr && text == "rm: invalid option: -z"
        ),
        "lines: {lines:?}"
    );
}

#[tokio::test]
async fn a_lone_dash_and_a_double_dash_stay_usage_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run_in("rm - a.txt", dir.path().to_path_buf()).await.0, 2);
    assert_eq!(run_in("rm -- a.txt", dir.path().to_path_buf()).await.0, 2);
}

#[tokio::test]
async fn test_negates_a_leading_bang() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "").unwrap();
    let cwd = dir.path().to_path_buf();
    // True branch: the negated condition is false, so `test !` succeeds.
    assert_eq!(run_in("test ! -f missing.txt", cwd.clone()).await.0, 0);
    // False branch: the negated condition is true, so `test !` fails.
    assert_eq!(run_in("test ! -f f.txt", cwd.clone()).await.0, 1);
    assert_eq!(run("test ! a = b && test ! 1 -gt 2").await.0, 0);
}

#[tokio::test]
async fn bracket_negates_a_leading_bang() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), "").unwrap();
    let cwd = dir.path().to_path_buf();
    assert_eq!(run_in("[ ! -f missing.txt ]", cwd.clone()).await.0, 0);
    assert_eq!(run_in("[ ! -f f.txt ]", cwd).await.0, 1);
}

#[tokio::test]
async fn a_negated_test_still_reports_a_malformed_operator() {
    // The `!` must not swallow the diagnostic for what follows it.
    let (code, lines) = run("test ! x -zz 1").await;
    assert_eq!(code, 2, "lines: {lines:?}");
}
