//! `sleep` and `test`/`[`: builtins with no filesystem mutation and no
//! stdout, only a delay (`sleep`) or a condition (`test`).

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::builtins::write_line;
use crate::interp::Flow;
use crate::io::OutTarget;

/// `sleep SECONDS`: `SECONDS` accepts a decimal (`0.2`). Genuinely
/// asynchronous rather than run on the blocking pool (see
/// `builtins::run_sleep`), so a cancellation during the wait stops it at
/// once instead of waiting for a blocking-pool thread to notice; a
/// cancelled sleep exits 130. Exactly one non-negative, finite operand
/// is accepted; anything else — no flags, so a `-`-prefixed argument
/// included — is a usage error, exit 2.
pub(crate) async fn sleep(args: &[String], stderr: OutTarget, cancel: &CancellationToken) -> Flow {
    let [only] = args else {
        write_line(
            stderr,
            format_args!("sleep: invalid duration: {}", args.join(" ")),
        );
        return Flow::Next(2);
    };
    // No flags at all: a leading `-` is rejected outright rather than
    // left to coincidentally fail `f64` parsing (which it would, since
    // every recognizable flag spelling is non-numeric) — the same
    // "unrecognized option" wording every other flag-less or
    // flag-bearing builtin uses.
    if only.starts_with('-') {
        write_line(stderr, format_args!("sleep: invalid option: {only}"));
        return Flow::Next(2);
    }
    let duration = only
        .parse::<f64>()
        .ok()
        .and_then(|seconds| std::time::Duration::try_from_secs_f64(seconds).ok());
    let Some(duration) = duration else {
        write_line(stderr, format_args!("sleep: invalid duration: {only}"));
        return Flow::Next(2);
    };

    tokio::select! {
        () = tokio::time::sleep(duration) => Flow::Next(0),
        () = cancel.cancelled() => Flow::Next(130),
    }
}

/// `test ARGS...`: no arguments is false; one argument tests
/// non-emptiness; two arguments are a unary operator (`-f -d -e -z -n`)
/// applied to the operand that follows; three are a left operand, a
/// binary operator (`= != -eq -ne -lt -le -gt -ge`), and a right
/// operand. The result is exit 0 (true) or 1 (false); a malformed
/// invocation — an unknown operator, a numeric operand that is not a
/// valid `i64`, or any other argument count — is a usage error, exit 2.
pub(crate) fn test(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    finish(evaluate(args, cwd), "test", stderr)
}

/// `[ ARGS... ]`: exactly `test`, but the final argument must be a
/// literal `]`, stripped before evaluating the rest. Its absence is a
/// usage error, exit 2.
pub(crate) fn test_bracket(args: &[String], cwd: &Path, stderr: OutTarget) -> Flow {
    match args.split_last() {
        Some((last, rest)) if last == "]" => finish(evaluate(rest, cwd), "[", stderr),
        _ => {
            write_line(stderr, "[: missing closing ]");
            Flow::Next(2)
        }
    }
}

fn finish(result: Result<bool, String>, name: &str, stderr: OutTarget) -> Flow {
    match result {
        Ok(true) => Flow::Next(0),
        Ok(false) => Flow::Next(1),
        Err(detail) => {
            write_line(stderr, format_args!("{name}: {detail}"));
            Flow::Next(2)
        }
    }
}

fn evaluate(args: &[String], cwd: &Path) -> Result<bool, String> {
    match args {
        [] => Ok(false),
        [only] => Ok(!only.is_empty()),
        [op, operand] => unary(op, operand, cwd),
        [left, op, right] => binary(left, op, right),
        _ => Err("too many arguments".to_string()),
    }
}

fn unary(op: &str, operand: &str, cwd: &Path) -> Result<bool, String> {
    match op {
        "-f" => Ok(cwd.join(operand).is_file()),
        "-d" => Ok(cwd.join(operand).is_dir()),
        "-e" => Ok(cwd.join(operand).exists()),
        "-z" => Ok(operand.is_empty()),
        "-n" => Ok(!operand.is_empty()),
        _ => Err(format!("unknown unary operator: {op}")),
    }
}

fn binary(left: &str, op: &str, right: &str) -> Result<bool, String> {
    match op {
        "=" => Ok(left == right),
        "!=" => Ok(left != right),
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let left = parse_integer(left)?;
            let right = parse_integer(right)?;
            Ok(match op {
                "-eq" => left == right,
                "-ne" => left != right,
                "-lt" => left < right,
                "-le" => left <= right,
                "-gt" => left > right,
                "-ge" => left >= right,
                _ => unreachable!("matched above"),
            })
        }
        _ => Err(format!("unknown binary operator: {op}")),
    }
}

fn parse_integer(operand: &str) -> Result<i64, String> {
    operand
        .parse()
        .map_err(|_| format!("integer expression expected: {operand}"))
}
