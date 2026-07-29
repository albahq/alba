//! The cross-platform conformance suite: every frozen semantic as a
//! script, its exit code, and its stdout lines, with no `cfg` anywhere in
//! the expectations.
//!
//! Built without the test harness (`harness = false` in `Cargo.toml`) so
//! this same binary can double as the external command some cases spawn:
//! invoked with [`HELPER_FLAG`] it prints its marker and exits, and a
//! case reaches it by substituting [`HELPER_PLACEHOLDER`] for
//! `current_exe()`. That is the only portable way to put a real process
//! behind a conformance case — no platform ships a command all three have
//! — and it is what keeps `spawn.rs`'s process-group handling, PATH-free
//! explicit-path resolution, environment passing, and line streaming
//! inside the suite that exists to prove they behave identically
//! everywhere. Under the harness the helper's output would arrive buried
//! in libtest's own, which no frozen expectation could match.

use std::path::PathBuf;

use alba_shell::{ShellEnv, ShellStream, execute, parse};
use tokio_util::sync::CancellationToken;

/// The argument that turns this binary into the external command a case
/// spawns instead of the suite that spawns it.
const HELPER_FLAG: &str = "--alba-shell-conformance-helper";

/// What a case writes where the helper's path belongs. Substituted, and
/// single-quoted in the script, so a path with a space or a windows
/// backslash in it stays one literal word.
const HELPER_PLACEHOLDER: &str = "{HELPER}";

/// The environment variable the helper echoes back when it is set, so a
/// case can prove the child really received the environment the shell
/// composed for it.
const HELPER_ECHO_VAR: &str = "ALBA_MARK";

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
/// A script containing [`HELPER_PLACEHOLDER`] has it replaced with the
/// path of this binary before it runs.
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
    Case {
        // A command substitution keeps its interior newlines and loses
        // only the trailing ones: quoted, the captured two lines stay two
        // lines, with no third empty one from a newline that survived.
        name: "subst_keeps_interior_newlines",
        script: "echo \"$(cat a.txt b.txt)\"",
        want_exit: 0,
        want_stdout: &["alpha", "beta"],
    },
    Case {
        // Unquoted, those same interior newlines are field separators
        // like any other whitespace, so the two lines become two
        // arguments on one line.
        name: "subst_interior_newlines_split_unquoted",
        script: "echo $(cat a.txt b.txt)",
        want_exit: 0,
        want_stdout: &["alpha beta"],
    },
    Case {
        // Tilde expansion: only at the start of a word, only before a `/`
        // or the end of the word, and never for `~user`. `HOME` is set by
        // the script itself so the expected value is the same everywhere.
        name: "tilde_expansion",
        script: "HOME=/alba/home; echo ~/x ~ ~foo",
        want_exit: 0,
        want_stdout: &["/alba/home/x /alba/home ~foo"],
    },
    Case {
        // A real external process: spawned from an explicit path, given
        // the environment the shell composed (assignment prefix
        // included), and its output split into lines on the way back.
        name: "external_command",
        script: "ALBA_MARK=beacon '{HELPER}' --alba-shell-conformance-helper",
        want_exit: 0,
        want_stdout: &["external command ok", "beacon"],
    },
    Case {
        // The same external feeding a builtin through a real pipe, and
        // its exit code left to the pipeline's last stage.
        name: "external_command_into_a_pipeline",
        script: "'{HELPER}' --alba-shell-conformance-helper | cat",
        want_exit: 0,
        want_stdout: &["external command ok"],
    },
    Case {
        // An external's own exit code reaches the shell unchanged, and
        // drives `||` like any builtin's would.
        name: "external_command_exit_code",
        script: "'{HELPER}' --alba-shell-conformance-helper --fail || echo recovered",
        want_exit: 0,
        want_stdout: &["external command ok", "recovered"],
    },
];

/// The external command the spawning cases reach: one marker line, the
/// value of [`HELPER_ECHO_VAR`] when the shell passed it through, and
/// exit 3 on `--fail`. Deliberately free of any dependency on this
/// crate, so what it proves is what a real external process does.
fn run_as_helper() -> i32 {
    println!("external command ok");
    if let Ok(mark) = std::env::var(HELPER_ECHO_VAR) {
        println!("{mark}");
    }
    if std::env::args().any(|arg| arg == "--fail") {
        return 3;
    }
    0
}

fn main() {
    if std::env::args().any(|arg| arg == HELPER_FLAG) {
        std::process::exit(run_as_helper());
    }

    let helper = std::env::current_exe().expect("the running test binary has a path");
    let helper = helper.display().to_string();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(conformance(&helper));

    println!("conformance: {} cases passed", CASES.len());
}

async fn conformance(helper: &str) {
    for case in CASES {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();
        std::fs::write(dir.path().join(".hidden"), "shh\n").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), "gamma\n").unwrap();
        let script = case.script.replace(HELPER_PLACEHOLDER, helper);
        let (code, lines) = run_in(&script, dir.path().to_path_buf()).await;
        assert_eq!(
            code, case.want_exit,
            "{}: exit code (lines: {lines:?})",
            case.name
        );
        assert_eq!(stdout(&lines), case.want_stdout.to_vec(), "{}", case.name);
    }
}
