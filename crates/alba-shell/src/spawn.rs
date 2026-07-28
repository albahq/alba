//! Spawns a resolved external binary, streaming its stdout/stderr
//! line-by-line back through `ShellEnv.output` and honouring
//! cancellation with a graceful-then-forceful termination.
//!
//! Ported from `alba-executors::shell::SystemShellExecutor`: this crate
//! cannot depend on `alba-executors` (it must stay standalone), but the
//! process-spawning, line-streaming, and cancellation-escalation
//! behaviour is the same — just wired to a resolved external binary
//! instead of `sh -c`/`powershell -Command`.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::{ShellOutputLine, ShellStream};

/// How long to wait, after sending the platform's "please stop" signal,
/// before escalating to a forceful kill.
const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// How long to wait for the stdout/stderr reader tasks to observe EOF,
/// after the child has exited or been killed, before giving up on
/// further output and returning anyway.
///
/// This is a backstop independent of cancellation: even an uncancelled
/// command that merely backgrounds something (`sleep 30 &`) can leave a
/// descendant holding the inherited stdout/stderr pipes open
/// indefinitely, which would otherwise block forever waiting for an EOF
/// that never comes. Combined with putting the child in its own process
/// group and signalling the whole group on cancellation (see
/// `build_command` and `send_terminate_signal`), this should essentially
/// never fire in practice — it exists for descendants that escape the
/// group (a double-forked/daemonized process) or simply outlive an
/// uncancelled run. Chosen short relative to `GRACE_PERIOD` because it
/// is not part of the "ask nicely, then insist" cancellation escalation;
/// it only bounds how long we wait to flush trailing output that should
/// already be sitting in the pipe by the time the process we care about
/// has exited.
const DRAIN_PERIOD: Duration = Duration::from_secs(2);

/// Runs `path` with `args` and `env` as its complete environment, cwd
/// `cwd`, streaming stdout/stderr line-by-line to `output` and
/// honouring `cancel` with a graceful-then-forceful termination.
///
/// Returns the child's exit code; a spawn failure (the resolved path
/// exists but cannot be executed) reports a stderr line and returns
/// `126`. A process killed by a signal (unix) reports `-1`, exactly as
/// `SystemShellExecutor` does — the caller (`interp.rs`) is responsible
/// for turning a cancelled run into the shell-level exit code 130.
pub(crate) async fn run_external(
    path: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> i32 {
    let mut child = match build_command(path, args, env, cwd).spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = output.send(ShellOutputLine {
                stream: ShellStream::Stderr,
                text: format!("alba-shell: failed to spawn {}: {error}", path.display()),
            });
            return 126;
        }
    };

    let stdout = child
        .stdout
        .take()
        .expect("child spawned with piped stdout");
    let stderr = child
        .stderr
        .take()
        .expect("child spawned with piped stderr");

    let mut stdout_task = tokio::spawn(stream_lines(stdout, ShellStream::Stdout, output.clone()));
    let mut stderr_task = tokio::spawn(stream_lines(stderr, ShellStream::Stderr, output.clone()));

    let status: Option<ExitStatus> = tokio::select! {
        result = child.wait() => result.ok(),
        _ = cancel.cancelled() => terminate(&mut child).await,
    };

    // The child has exited (or been reaped after termination), so its
    // pipes are closing/closed; let the reader tasks drain whatever is
    // left, but only for up to DRAIN_PERIOD total — see its doc comment
    // for why this must be bounded rather than an unconditional await.
    // Both tasks share a single DRAIN_PERIOD budget (joined concurrently,
    // not one after another): awaiting them sequentially would double
    // the worst-case wait to `2 * DRAIN_PERIOD` for no benefit, since
    // both pipes can stall for the same reason (a surviving descendant)
    // at the same time.
    let drain = async {
        let _ = tokio::join!(&mut stdout_task, &mut stderr_task);
    };
    if tokio::time::timeout(DRAIN_PERIOD, drain).await.is_err() {
        stdout_task.abort();
        stderr_task.abort();
    }

    status.and_then(|s| s.code()).unwrap_or(-1)
}

fn build_command(path: &Path, args: &[String], env: &[(String, String)], cwd: &Path) -> Command {
    let mut command = Command::new(path);
    command.args(args);

    // Make this child the leader of its own process group (pgid == its
    // pid) instead of inheriting ours. An external command can itself
    // be compound or backgrounding — without its own group, cancellation
    // could only ever signal the immediate child, leaving whatever it
    // spawned running. See `send_terminate_signal`/`force_kill`, which
    // signal the negated pid to reach the whole group.
    #[cfg(unix)]
    command.process_group(0);

    command
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Reads `reader` to EOF, splitting on `\n` and stripping a `\r` that
/// immediately precedes it (the CRLF line endings a windows child
/// produces), sending one [`ShellOutputLine`] per line. A final line
/// with no trailing newline is still emitted; lines of any length are
/// supported since the buffer grows as needed rather than being capped.
async fn stream_lines<R>(reader: R, stream: ShellStream, output: UnboundedSender<ShellOutputLine>)
where
    R: AsyncRead + Unpin,
{
    let mut reader = tokio::io::BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf).await {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        }
        let text = String::from_utf8_lossy(&buf).into_owned();
        // If the receiver has been dropped, nobody is listening for
        // output anymore; keep draining the pipe regardless so the
        // child is never blocked writing into a full pipe while it
        // still runs.
        let _ = output.send(ShellOutputLine { stream, text });
    }
}

/// Terminates a still-running child after cancellation: send the
/// platform's graceful-stop request, wait up to [`GRACE_PERIOD`] for it
/// to exit on its own, then escalate to an unconditional kill.
///
/// On unix both steps target the child's whole process group (it was
/// spawned as that group's leader — see `build_command`), so a compound
/// or backgrounding command dies together with the process we directly
/// hold, not just that one process. The graceful request is a real
/// `SIGTERM`, distinct from the forceful `SIGKILL` used on escalation.
///
/// Windows has no equivalent of `SIGTERM`/process groups for an
/// arbitrary process short of a Job object (`CREATE_NEW_PROCESS_GROUP`
/// plus `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`), which this crate does not
/// set up: `TerminateProcess` (what `Child::start_kill` issues) is
/// unconditional and reaches only the immediate child, not its
/// descendants. Both steps use that same forceful, child-only
/// termination on windows; the grace period is harmless but in practice
/// never has anything left to wait out. This is a known, deliberate gap
/// — an external command on windows that backgrounds a descendant can
/// outlive cancellation there.
async fn terminate(child: &mut Child) -> Option<ExitStatus> {
    send_terminate_signal(child);

    match tokio::time::timeout(GRACE_PERIOD, child.wait()).await {
        Ok(result) => result.ok(),
        Err(_elapsed) => {
            force_kill(child).await;
            child.wait().await.ok()
        }
    }
}

#[cfg(unix)]
fn send_terminate_signal(child: &Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    if let Some(pid) = child.id() {
        // The negated pid targets the whole process group this child
        // leads (see `build_command`'s `process_group(0)`), not just the
        // child itself. Best-effort: if the group already exited between
        // us checking and sending, `kill` returning an error (ESRCH) is
        // fine — the subsequent `wait` will observe the exit either way.
        let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
    }
}

#[cfg(windows)]
fn send_terminate_signal(child: &mut Child) {
    let _ = child.start_kill();
}

/// The forceful escalation after [`GRACE_PERIOD`] elapses. On unix this
/// is a group-wide `SIGKILL`, mirroring [`send_terminate_signal`]'s
/// group-wide `SIGTERM`. On windows it is `Child::kill`, which only
/// reaches the immediate child (see `terminate`'s doc comment for why).
#[cfg(unix)]
async fn force_kill(child: &Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    if let Some(pid) = child.id() {
        let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
    }
}

#[cfg(windows)]
async fn force_kill(child: &mut Child) {
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    async fn collect(data: &[u8]) -> Vec<String> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        stream_lines(Cursor::new(data.to_vec()), ShellStream::Stdout, tx).await;
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line.text);
        }
        lines
    }

    #[tokio::test]
    async fn strips_trailing_cr_from_crlf_line_endings() {
        assert_eq!(collect(b"hello\r\nworld\r\n").await, vec!["hello", "world"]);
    }

    #[tokio::test]
    async fn emits_a_final_line_with_no_trailing_newline() {
        assert_eq!(collect(b"first\nsecond").await, vec!["first", "second"]);
    }

    #[tokio::test]
    async fn empty_input_emits_no_lines() {
        assert_eq!(collect(b"").await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_lone_cr_not_followed_by_newline_is_preserved() {
        // Only a `\r` immediately preceding the `\n` we split on is a
        // line terminator artifact; a `\r` elsewhere in the line is
        // real content.
        assert_eq!(collect(b"a\rb\n").await, vec!["a\rb"]);
    }

    #[tokio::test]
    async fn handles_a_very_long_line() {
        let long_line = "x".repeat(200_000);
        let mut data = long_line.clone().into_bytes();
        data.push(b'\n');
        assert_eq!(collect(&data).await, vec![long_line]);
    }
}
