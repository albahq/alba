//! State-affecting builtins: `cd`, `pwd`, `exit`, `true`, `false`,
//! `export`, `unset`. Builtins always win over PATH binaries of the
//! same name — `interp::exec_command` looks them up before falling back
//! to an external — and an explicit path (`/bin/echo`) bypasses them.
//! Task 6 extends this with output-producing builtins (`echo`, …).

use crate::interp::{Flow, Lines};
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
}

/// Every builtin name, fed both by [`find`] and the did-you-mean
/// suggestion in `interp.rs`'s command-not-found diagnostic.
pub(crate) const NAMES: &[&str] = &["cd", "pwd", "exit", "true", "false", "export", "unset"];

pub(crate) fn find(name: &str) -> Option<Builtin> {
    match name {
        "cd" => Some(Builtin::Cd),
        "pwd" => Some(Builtin::Pwd),
        "exit" => Some(Builtin::Exit),
        "true" => Some(Builtin::True),
        "false" => Some(Builtin::False),
        "export" => Some(Builtin::Export),
        "unset" => Some(Builtin::Unset),
        _ => None,
    }
}

pub(crate) fn run(builtin: Builtin, args: &[String], state: &mut ShellState, io: &Lines) -> Flow {
    match builtin {
        Builtin::True => Flow::Next(0),
        Builtin::False => Flow::Next(1),
        Builtin::Exit => Flow::Exit(exit_code(args, state)),
        Builtin::Pwd => {
            io.stdout(state.cwd.display().to_string());
            Flow::Next(0)
        }
        Builtin::Cd => cd(args, state, io),
        Builtin::Export => export(args, state, io),
        Builtin::Unset => {
            for name in args {
                state.unset(name);
            }
            Flow::Next(0)
        }
    }
}

/// `exit [n]`: an explicit `n` must parse as an integer; with no
/// argument, the code of the last completed command.
fn exit_code(args: &[String], state: &ShellState) -> i32 {
    match args.first() {
        Some(n) => n.parse().unwrap_or(state.last_exit),
        None => state.last_exit,
    }
}

fn cd(args: &[String], state: &mut ShellState, io: &Lines) -> Flow {
    let target = match args.first() {
        Some(path) => path.clone(),
        None => match state.home() {
            Some(home) => home.to_string(),
            None => {
                io.stderr("cd: HOME not set");
                return Flow::Next(1);
            }
        },
    };

    match state.cwd.join(&target).canonicalize() {
        Ok(resolved) if resolved.is_dir() => {
            // Keep `PWD` in step with the real cwd: it is exported, so a
            // spawned child's own `current_dir` and the `PWD` it inherits
            // must agree, and (once Task 4 adds variable expansion)
            // `$PWD` inside this shell must reflect where `cd` actually
            // moved to, not the directory the run started in.
            state.export("PWD", Some(resolved.display().to_string()));
            state.cwd = resolved;
            Flow::Next(0)
        }
        _ => {
            io.stderr(format!("cd: no such directory: {target}"));
            Flow::Next(1)
        }
    }
}

/// `export NAME[=VALUE]...`: marks each name exported, setting its
/// value when `=VALUE` is given. An invalid identifier is a usage
/// error: stderr and exit 2, without processing the remaining names.
fn export(args: &[String], state: &mut ShellState, io: &Lines) -> Flow {
    for arg in args {
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if !is_valid_name(name) {
            io.stderr(format!("export: not a valid identifier: {name}"));
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
