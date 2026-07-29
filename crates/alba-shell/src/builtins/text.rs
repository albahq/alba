//! Output builtins that read and write shell streams: `echo` and `cat`.

use std::io::Write;
use std::path::Path;

use crate::interp::Flow;
use crate::io::{CommandIo, OutTarget};

/// `echo [-n] args...`: arguments joined by a single space, no escape
/// sequences interpreted, always exits 0. `-n` suppresses the trailing
/// newline, but only counts as a flag when it is the very first
/// argument — `echo -e y` prints `-e y` verbatim, since `-e` is not `-n`
/// and a `-n` past the first position is just another argument.
pub(crate) fn echo(args: &[String], stdout: OutTarget) -> Flow {
    let (suppress_newline, rest) = match args.first() {
        Some(first) if first == "-n" => (true, &args[1..]),
        _ => (false, args),
    };

    let mut writer = stdout.writer();
    let text = rest.join(" ");
    if suppress_newline {
        let _ = write!(writer, "{text}");
    } else {
        let _ = writeln!(writer, "{text}");
    }
    Flow::Next(0)
}

/// `cat [file...]`: with no arguments, copies stdin to stdout verbatim.
/// Each argument names a file resolved against `cwd`; a file that
/// cannot be opened reports `cat: NAME: no such file` and `cat` moves on
/// to the next one rather than stopping. Exits 1 if any file failed, 0
/// otherwise (always 0 with no arguments, whatever stdin held).
pub(crate) fn cat(args: &[String], cwd: &Path, io: CommandIo) -> Flow {
    let CommandIo {
        stdin,
        stdout,
        stderr,
    } = io;
    let mut out = stdout.writer();

    if args.is_empty() {
        let mut input = stdin.reader();
        let _ = std::io::copy(&mut input, &mut out);
        return Flow::Next(0);
    }
    // No file argument reads stdin; drop it now rather than at the end
    // of the loop below, so an upstream pipeline producer is never left
    // waiting on a reader that was never going to drain it.
    drop(stdin);

    let mut err = stderr.writer();
    let mut failed = false;
    for arg in args {
        match std::fs::File::open(cwd.join(arg)) {
            Ok(mut file) => {
                if std::io::copy(&mut file, &mut out).is_err() {
                    failed = true;
                }
            }
            Err(_) => {
                failed = true;
                let _ = writeln!(err, "cat: {arg}: no such file");
            }
        }
    }
    Flow::Next(if failed { 1 } else { 0 })
}
