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

fn stdout(lines: &[(ShellStream, String)]) -> Vec<&str> {
    lines
        .iter()
        .filter(|(s, _)| *s == ShellStream::Stdout)
        .map(|(_, t)| t.as_str())
        .collect()
}

/// One frozen semantic: a script, the exit code it must produce, and the
/// stdout lines it must produce, exercised fresh in its own tempdir
/// against the standard fixture tree so no case can see another's state.
struct Case {
    name: &'static str,
    script: &'static str,
    want_exit: i32,
    want_stdout: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        name: "echo_joins",
        script: "echo one  two",
        want_exit: 0,
        want_stdout: &["one two"],
    },
    Case {
        name: "quoting_preserves",
        script: "echo 'a  b' \"c  d\"",
        want_exit: 0,
        want_stdout: &["a  b c  d"],
    },
    Case {
        name: "var_roundtrip",
        script: "X=alba; echo $X${X}",
        want_exit: 0,
        want_stdout: &["albaalba"],
    },
    Case {
        name: "unset_var_is_empty",
        script: "echo start${NOPE}end",
        want_exit: 0,
        want_stdout: &["startend"],
    },
    Case {
        name: "last_exit_status",
        script: "false; echo $?; true; echo $?",
        want_exit: 0,
        want_stdout: &["1", "0"],
    },
    Case {
        name: "subst",
        script: "echo $(echo inner)",
        want_exit: 0,
        want_stdout: &["inner"],
    },
    Case {
        name: "glob_sorted_forward_slashes",
        script: "echo sub/*.txt *.txt",
        want_exit: 0,
        want_stdout: &["sub/c.txt a.txt b.txt"],
    },
    Case {
        // A wildcard never reaches a hidden entry, so `rm -r -f *` in a
        // cleanup beam cannot take `.git` with it.
        name: "glob_skips_hidden_entries",
        script: "echo *",
        want_exit: 0,
        want_stdout: &["a.txt b.txt sub"],
    },
    Case {
        // The other half of the same rule: a dot written out still
        // reaches what it names.
        name: "glob_with_a_written_dot_matches_hidden",
        script: "echo .h*",
        want_exit: 0,
        want_stdout: &[".hidden"],
    },
    Case {
        name: "unmatched_glob_literal",
        script: "echo *.zzz",
        want_exit: 0,
        want_stdout: &["*.zzz"],
    },
    Case {
        name: "quoted_glob_literal",
        script: "echo '*.txt'",
        want_exit: 0,
        want_stdout: &["*.txt"],
    },
    Case {
        name: "pipeline",
        script: "cat a.txt b.txt | cat",
        want_exit: 0,
        want_stdout: &["alpha", "beta"],
    },
    Case {
        name: "pipeline_exit_is_last",
        script: "false | true",
        want_exit: 0,
        want_stdout: &[],
    },
    Case {
        name: "and_or",
        script: "test -f a.txt && echo yes || echo no",
        want_exit: 0,
        want_stdout: &["yes"],
    },
    Case {
        name: "test_bang",
        script: "test ! -f missing.txt && [ ! -d missing ] && echo both",
        want_exit: 0,
        want_stdout: &["both"],
    },
    Case {
        name: "negation",
        script: "! test -f missing.txt",
        want_exit: 0,
        want_stdout: &[],
    },
    Case {
        name: "redirect_then_cat",
        script: "echo saved > out.txt && cat out.txt",
        want_exit: 0,
        want_stdout: &["saved"],
    },
    Case {
        name: "append",
        script: "echo 1 > o.txt; echo 2 >> o.txt; cat o.txt",
        want_exit: 0,
        want_stdout: &["1", "2"],
    },
    Case {
        name: "stdin_redirect",
        script: "cat < a.txt",
        want_exit: 0,
        want_stdout: &["alpha"],
    },
    Case {
        name: "cp_mv_rm_roundtrip",
        script: "cp a.txt c.txt && mv c.txt d.txt && rm d.txt && test -f a.txt",
        want_exit: 0,
        want_stdout: &[],
    },
    Case {
        name: "clean_rebuild_dir",
        script: "rm -r -f dist && mkdir dist && test -d dist",
        want_exit: 0,
        want_stdout: &[],
    },
    Case {
        // The grouped spelling every real script writes: `-rf` must mean
        // exactly `-r -f`, on every platform.
        name: "clean_rebuild_dir_grouped_flags",
        script: "rm -rf dist && mkdir dist && test -d dist",
        want_exit: 0,
        want_stdout: &[],
    },
    Case {
        name: "exit_code_propagates",
        script: "exit 4",
        want_exit: 4,
        want_stdout: &[],
    },
    Case {
        name: "not_found_is_127",
        script: "definitely-not-a-command-alba",
        want_exit: 127,
        want_stdout: &[],
    },
    Case {
        name: "assignment_scoped_to_line",
        script: "X=1; echo $X",
        want_exit: 0,
        want_stdout: &["1"],
    },
    Case {
        name: "newline_is_semicolon",
        script: "echo a\necho b",
        want_exit: 0,
        want_stdout: &["a", "b"],
    },
];

#[tokio::test]
async fn conformance() {
    for case in CASES {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        std::fs::write(dir.path().join(".hidden"), "shh\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "gamma\n").unwrap();
        let (code, lines) = run_in(case.script, dir.path().to_path_buf()).await;
        assert_eq!(
            code, case.want_exit,
            "{}: exit code (lines: {lines:?})",
            case.name
        );
        assert_eq!(stdout(&lines), case.want_stdout.to_vec(), "{}", case.name);
    }
}
