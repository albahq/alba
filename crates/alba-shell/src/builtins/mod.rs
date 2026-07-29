//! Every shell builtin. State-affecting ones (`cd`, `pwd`, `exit`,
//! `true`, `false`, `export`, `unset`) live directly in this module; the
//! rest are split by responsibility into their own modules: [`text`]
//! (`echo`, `cat`), [`fs`] (`cp`, `mv`, `rm`, `mkdir`, `touch`), and
//! [`util`] (`sleep`, `test`/`[`). Builtins always win over PATH
//! binaries of the same name — `interp::exec_command` looks them up
//! before falling back to an external — and an explicit path
//! (`/bin/echo`) bypasses them.

mod fs;
mod text;
mod util;

use std::io::Write;

use tokio_util::sync::CancellationToken;

use crate::interp::Flow;
use crate::io::{CommandIo, OutTarget};
use crate::state::ShellState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    Cd,
    Pwd,
    Exit,
    True,
    False,
    Export,
    Unset,
    Echo,
    Cat,
    Cp,
    Mv,
    Rm,
    Mkdir,
    Touch,
    Sleep,
    Test,
    Bracket,
}

/// Every builtin name except `[` — an alternate spelling of `test`, not
/// a name a did-you-mean typo should ever suggest — fed both by
/// [`find`] and the did-you-mean suggestion in `interp.rs`'s
/// command-not-found diagnostic.
pub(crate) const NAMES: &[&str] = &[
    "cd", "pwd", "exit", "true", "false", "export", "unset", "echo", "cat", "cp", "mv", "rm",
    "mkdir", "touch", "sleep", "test",
];

pub(crate) fn find(name: &str) -> Option<Builtin> {
    match name {
        "cd" => Some(Builtin::Cd),
        "pwd" => Some(Builtin::Pwd),
        "exit" => Some(Builtin::Exit),
        "true" => Some(Builtin::True),
        "false" => Some(Builtin::False),
        "export" => Some(Builtin::Export),
        "unset" => Some(Builtin::Unset),
        "echo" => Some(Builtin::Echo),
        "cat" => Some(Builtin::Cat),
        "cp" => Some(Builtin::Cp),
        "mv" => Some(Builtin::Mv),
        "rm" => Some(Builtin::Rm),
        "mkdir" => Some(Builtin::Mkdir),
        "touch" => Some(Builtin::Touch),
        "sleep" => Some(Builtin::Sleep),
        "test" => Some(Builtin::Test),
        "[" => Some(Builtin::Bracket),
        _ => None,
    }
}

/// Runs `builtin` on the streams `io` describes. Synchronous by design:
/// a builtin's writes may land on a pipe or a file, which blocks, so
/// `interp::run_builtin` decides whether to call this inline or on the
/// blocking pool. `sleep` is the one exception — genuinely asynchronous
/// and cancellable — and is never routed through here; see
/// [`run_sleep`].
pub(crate) fn run(
    builtin: Builtin,
    args: &[String],
    state: &mut ShellState,
    io: CommandIo,
) -> Flow {
    // `cat` is the one builtin that reads its stdin, so it alone keeps
    // the whole `io` rather than having it destructured away below.
    if builtin == Builtin::Cat {
        return text::cat(args, &state.cwd, io);
    }

    // No other builtin here reads its input. Letting `stdin` drop right
    // away closes this end of an upstream pipe, so a producer in `cmd |
    // pwd` is not left blocked writing into a pipe nobody will ever read.
    let CommandIo {
        stdin: _,
        stdout,
        stderr,
    } = io;

    match builtin {
        Builtin::True => Flow::Next(0),
        Builtin::False => Flow::Next(1),
        Builtin::Exit => exit(args, state, stderr),
        Builtin::Pwd => {
            write_line(stdout, state.cwd.display());
            Flow::Next(0)
        }
        Builtin::Cd => cd(args, state, stderr),
        Builtin::Export => export(args, state, stderr),
        Builtin::Unset => {
            for name in args {
                state.unset(name);
            }
            Flow::Next(0)
        }
        Builtin::Echo => text::echo(args, stdout),
        Builtin::Cp => fs::cp(args, &state.cwd, stderr),
        Builtin::Mv => fs::mv(args, &state.cwd, stderr),
        Builtin::Rm => fs::rm(args, &state.cwd, stderr),
        Builtin::Mkdir => fs::mkdir(args, &state.cwd, stderr),
        Builtin::Touch => fs::touch(args, &state.cwd, stderr),
        Builtin::Test => util::test(args, &state.cwd, stderr),
        Builtin::Bracket => util::test_bracket(args, &state.cwd, stderr),
        Builtin::Cat => unreachable!("handled above, before `io` was destructured"),
        Builtin::Sleep => unreachable!("dispatched asynchronously; see `run_sleep`"),
    }
}

/// Runs the `sleep` builtin: genuinely asynchronous rather than
/// blocking, so a cancellation lands the instant it fires instead of
/// waiting for a blocking-pool thread to notice it. Kept out of [`run`]
/// for exactly that reason — `interp::run_builtin` calls this directly,
/// before it ever considers the blocking pool.
pub(crate) async fn run_sleep(args: &[String], io: CommandIo, cancel: &CancellationToken) -> Flow {
    let CommandIo {
        stdin: _,
        stdout: _,
        stderr,
    } = io;
    util::sleep(args, stderr, cancel).await
}

/// Writes one newline-terminated line to `target` and closes it. A
/// failed write is deliberately ignored: a builtin whose output goes
/// nowhere (a closed channel, a pipe whose reader has gone) still
/// succeeds, exactly as it does when the receiving end of the run's
/// output channel has been dropped.
pub(crate) fn write_line(target: OutTarget, text: impl std::fmt::Display) {
    let mut writer = target.writer();
    let _ = writeln!(writer, "{text}");
}

/// A builtin usage error: `NAME: <detail>` on stderr, exit 2 — the
/// flag/argument surface each builtin freezes.
pub(crate) fn usage_error(stderr: OutTarget, message: impl std::fmt::Display) -> Flow {
    write_line(stderr, message);
    Flow::Next(2)
}

/// A builtin runtime error: `NAME: <detail>` on stderr, exit 1 — a
/// well-formed invocation that failed to do what it asked (a missing
/// file, a directory in the way, an OS error).
pub(crate) fn command_error(stderr: OutTarget, message: impl std::fmt::Display) -> Flow {
    write_line(stderr, message);
    Flow::Next(1)
}

/// Consumes leading flags from `known` off the front of `args`, in any
/// order, stopping at the first argument that is not one of them.
///
/// A `-`-prefixed argument is decomposed letter by letter, so `-rf` is
/// exactly `-r -f` and `-pv` names `-p` and `-v` separately: the grouped
/// spelling is what real scripts write, and treating it as one opaque
/// option would reject `rm -rf` outright. The error carries the single
/// option that was not recognized (`-z` for `rm -rz`), never the group it
/// was written in, so the message blames the right letter.
///
/// A lone `-` and a `--` are both rejected: neither means anything to any
/// builtin here (none reads standard input by name, and none has enough
/// of a flag surface for an end-of-options marker to be worth freezing).
///
/// `known: &[]` still has a job: a builtin with no flag surface at all
/// (`mv`, `touch`, `cat`) calls this the same way, and it rejects any
/// leading `-`-prefixed argument as unrecognized — the frozen rule
/// ("anything outside a builtin's documented flag surface is a usage
/// error") applies precisely because their surface is empty, not
/// despite it.
pub(crate) fn take_flags<'a, 'k>(
    args: &'a [String],
    known: &'k [&'k str],
) -> Result<(Vec<&'k str>, &'a [String]), String> {
    let mut seen = Vec::new();
    let mut rest = args;
    while let Some(first) = rest.first() {
        let Some(letters) = first.strip_prefix('-') else {
            break;
        };
        if letters.is_empty() {
            return Err(first.clone());
        }
        for letter in letters.chars() {
            let spelled = format!("-{letter}");
            match known.iter().copied().find(|flag| *flag == spelled) {
                Some(flag) => seen.push(flag),
                None => return Err(spelled),
            }
        }
        rest = &rest[1..];
    }
    Ok((seen, rest))
}

/// `exit [n]`: an explicit `n` must parse as an integer; with no
/// argument, the code of the last completed command.
///
/// A non-numeric `n` is a usage error like any other, exit 2, rather
/// than a silent fall back to the last code — which would let `exit
/// $VERSION` on an unset variable end a beam in success. The program
/// still stops: `exit` always exits, whatever it was handed.
fn exit(args: &[String], state: &ShellState, stderr: OutTarget) -> Flow {
    match args.first() {
        None => Flow::Exit(state.last_exit),
        Some(code) => match code.parse() {
            Ok(code) => Flow::Exit(code),
            Err(_) => {
                write_line(
                    stderr,
                    format_args!("exit: numeric argument required: {code}"),
                );
                Flow::Exit(2)
            }
        },
    }
}

fn cd(args: &[String], state: &mut ShellState, stderr: OutTarget) -> Flow {
    let target = match args.first() {
        Some(path) => path.clone(),
        None => match state.home() {
            Some(home) => home.to_string(),
            None => {
                write_line(stderr, "cd: HOME not set");
                return Flow::Next(1);
            }
        },
    };

    match state.cwd.join(&target).canonicalize() {
        Ok(resolved) if resolved.is_dir() => {
            // Keep `PWD` in step with the real cwd: it is exported, so a
            // spawned child's own `current_dir` and the `PWD` it inherits
            // must agree, and `$PWD` inside this shell must reflect where
            // `cd` actually moved to, not the directory the run started
            // in.
            state.export("PWD", Some(resolved.display().to_string()));
            state.cwd = resolved;
            Flow::Next(0)
        }
        _ => {
            write_line(stderr, format_args!("cd: no such directory: {target}"));
            Flow::Next(1)
        }
    }
}

/// `export NAME[=VALUE]...`: marks each name exported, setting its
/// value when `=VALUE` is given. An invalid identifier is a usage
/// error: stderr and exit 2, without processing the remaining names.
fn export(args: &[String], state: &mut ShellState, stderr: OutTarget) -> Flow {
    for arg in args {
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if !is_valid_name(name) {
            write_line(
                stderr,
                format_args!("export: not a valid identifier: {name}"),
            );
            return Flow::Next(2);
        }
        state.export(name, value);
    }
    Flow::Next(0)
}

/// POSIX shell variable name: `_`/alphabetic then `_`/alphanumeric.
fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}
