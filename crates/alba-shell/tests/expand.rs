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
async fn a_shell_variable_expands_in_a_later_command() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("D=sub; cd $D && pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
    assert!(stdout(&lines).join("").ends_with("sub"));
}

#[tokio::test]
async fn an_unset_variable_expands_to_nothing() {
    // An unset variable expands to nothing and, as its own word, drops
    // out of the argument list entirely rather than becoming an empty
    // argument: `echo` sees exactly `before` and `after`.
    let (code, lines) = run("echo before $NOPE_UNSET_VAR after").await;
    assert_eq!(code, 0);
    assert_eq!(stdout(&lines), vec!["before after"]);
}

#[tokio::test]
async fn double_quotes_prevent_field_splitting() {
    let (_, lines) = run(r#"A='x  y'; echo "$A""#).await;
    assert_eq!(stdout(&lines), vec!["x  y"]);
}

#[tokio::test]
async fn unquoted_expansion_field_splits() {
    let (_, lines) = run("A='x  y'; echo $A").await;
    assert_eq!(stdout(&lines), vec!["x y"]);
}

#[tokio::test]
async fn command_substitution_feeds_an_assignment() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let (code, lines) = run_in("D=$(pwd)/sub; cd $D && pwd", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
    assert!(stdout(&lines).join("").ends_with("sub"));
}

#[tokio::test]
async fn command_substitution_strips_trailing_newlines_only() {
    let dir = tempfile::tempdir().unwrap();
    // pwd emits one trailing newline; without stripping, `cd` would fail.
    let (code, _) = run_in("cd $(pwd)", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn substitution_stderr_reaches_the_outer_output() {
    let (_, lines) = run("X=$(definitely-not-a-command-alba); true").await;
    assert!(
        lines
            .iter()
            .any(|(s, t)| *s == ShellStream::Stderr && t.contains("command not found"))
    );
}

#[tokio::test]
async fn tilde_expands_to_home_at_word_start() {
    let (code, _) = run_in("cd ~", std::env::temp_dir()).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn a_single_match_glob_resolves_to_a_directory_cd_can_use() {
    // Not the sorting/forward-slash contract (see
    // `globs_sort_and_use_forward_slashes` below for that): this only
    // confirms a glob that matches exactly one entry expands to
    // something `cd` accepts when that entry is a directory.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("only_dir_here")).unwrap();
    let (code, _) = run_in("cd only_*", dir.path().to_path_buf()).await;
    assert_eq!(code, 0);
}

#[tokio::test]
async fn an_unmatched_glob_stays_literal() {
    let dir = tempfile::tempdir().unwrap();
    let (code, lines) = run_in("cd no_such_*", dir.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(lines.iter().any(|(_, t)| t.contains("no_such_*")));
}

#[tokio::test]
async fn an_absolute_glob_pattern_matches_and_yields_absolute_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("only_dir_here")).unwrap();
    // The cwd must not be an ancestor of the pattern, or a match stripped
    // of the cwd prefix would still resolve and the test would pass for
    // the wrong reason. A sibling tempdir guarantees no prefix relation.
    let elsewhere = tempfile::tempdir().unwrap();
    let src = format!("cd {}/only_*", dir.path().display());
    let (code, lines) = run_in(&src, elsewhere.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
}

#[tokio::test]
async fn an_unmatched_absolute_glob_stays_literal() {
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let pattern = format!("{}/no_such_*", dir.path().display());
    let (code, lines) = run_in(&format!("cd {pattern}"), elsewhere.path().to_path_buf()).await;
    assert_eq!(code, 1);
    assert!(
        lines.iter().any(|(_, t)| t.contains(&pattern)),
        "the unmatched absolute pattern must survive verbatim, lines: {lines:?}"
    );
}

#[tokio::test]
async fn an_escaped_space_does_not_split_a_field() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a b")).unwrap();
    let (code, lines) = run_in(r"cd a\ b", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
}

#[tokio::test]
async fn an_escaped_star_does_not_glob() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("foobar")).unwrap();
    let (code, lines) = run_in(r"cd foo\*", dir.path().to_path_buf()).await;
    assert_eq!(code, 1, "an escaped `*` must not match `foobar`");
    assert!(lines.iter().any(|(_, t)| t.contains("foo*")));
}

#[tokio::test]
async fn an_escaped_question_mark_does_not_glob() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("fooX")).unwrap();
    let (code, lines) = run_in(r"cd foo\?", dir.path().to_path_buf()).await;
    assert_eq!(code, 1, "an escaped `?` must not match `fooX`");
    assert!(lines.iter().any(|(_, t)| t.contains("foo?")));
}

// A tab and a backslash are both legal in a unix filename; on windows a
// backslash is a path separator and a tab is not portably creatable, so
// these two pin the escaping contract on unix only.
#[cfg(unix)]
#[tokio::test]
async fn an_escaped_tab_does_not_split_a_field() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a\tb")).unwrap();
    let (code, lines) = run_in("cd a\\\tb", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn an_escaped_backslash_is_one_literal_backslash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("a\\b")).unwrap();
    let (code, lines) = run_in(r"cd a\\b", dir.path().to_path_buf()).await;
    assert_eq!(code, 0, "lines: {lines:?}");
}

#[tokio::test]
async fn quoted_glob_characters_do_not_glob() {
    let (_, lines) = run(r#"echo "*""#).await;
    assert_eq!(stdout(&lines), vec!["*"]);
}

#[tokio::test]
async fn globs_sort_and_use_forward_slashes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    for name in ["d/b.rs", "d/a.rs"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    let (_, lines) = run_in("echo d/*.rs", dir.path().to_path_buf()).await;
    assert_eq!(stdout(&lines), vec!["d/a.rs d/b.rs"]);
}

#[tokio::test]
async fn a_star_never_matches_a_hidden_entry() {
    // The rule every POSIX shell freezes, and the one that keeps a
    // cleanup beam's `rm -rf *` away from `.git` and a `cp * dist` away
    // from `.env`.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".hidden"), "").unwrap();
    std::fs::write(dir.path().join("visible.txt"), "").unwrap();
    let (_, lines) = run_in("echo *", dir.path().to_path_buf()).await;
    assert_eq!(stdout(&lines), vec!["visible.txt"]);
}

#[tokio::test]
async fn a_leading_dot_written_out_still_matches_a_hidden_entry() {
    // Only an *implicit* leading dot is protected: `.*` asks for hidden
    // entries by name and must still find them.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".hidden"), "").unwrap();
    std::fs::write(dir.path().join("visible.txt"), "").unwrap();
    let (_, lines) = run_in("echo .h*", dir.path().to_path_buf()).await;
    assert_eq!(stdout(&lines), vec![".hidden"]);
}

#[tokio::test]
async fn a_star_never_matches_a_hidden_entry_inside_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("d")).unwrap();
    std::fs::write(dir.path().join("d/.hidden"), "").unwrap();
    std::fs::write(dir.path().join("d/visible.txt"), "").unwrap();
    let (_, lines) = run_in("echo d/*", dir.path().to_path_buf()).await;
    assert_eq!(stdout(&lines), vec!["d/visible.txt"]);
}

#[tokio::test]
async fn last_exit_status_expands_across_a_sequence() {
    let (_, lines) = run("false; echo $?; true; echo $?").await;
    assert_eq!(stdout(&lines), vec!["1", "0"]);
}

#[tokio::test]
async fn last_exit_status_expands_across_and_or_operators() {
    let (_, lines) = run("false || echo code=$?; true && echo code=$?").await;
    assert_eq!(stdout(&lines), vec!["code=1", "code=0"]);
}

#[tokio::test]
async fn last_exit_status_expands_inside_double_quotes() {
    let (_, lines) = run(r#"false; echo "status is $?""#).await;
    assert_eq!(stdout(&lines), vec!["status is 1"]);
}
