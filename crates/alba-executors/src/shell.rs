//! [`SystemShellExecutor`]: runs a [`CommandSpec`] through the host shell.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead};
use tokio::process::{Child, Command};

use crate::{CommandSpec, ExecContext, ExecError, ExecResult, Executor, OutputLine, Stream};

/// How long to wait, after sending the platform's "please stop" signal,
/// before escalating to a forceful kill.
const GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Runs commands via `sh -c` (unix) or `powershell -NoProfile -Command`
/// (windows), streaming stdout/stderr line-by-line and honouring
/// [`ExecContext::cancel`] with a graceful-then-forceful termination.
pub struct SystemShellExecutor;

#[async_trait::async_trait]
impl Executor for SystemShellExecutor {
    async fn execute(&self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError> {
        let mut child = build_command(&cmd).spawn().map_err(|error| ExecError {
            message: format!("failed to spawn `{}`: {error}", cmd.command),
        })?;

        let stdout = child
            .stdout
            .take()
            .expect("child spawned with piped stdout");
        let stderr = child
            .stderr
            .take()
            .expect("child spawned with piped stderr");

        let stdout_task = tokio::spawn(stream_lines(stdout, Stream::Stdout, ctx.output.clone()));
        let stderr_task = tokio::spawn(stream_lines(stderr, Stream::Stderr, ctx.output.clone()));

        let status = tokio::select! {
            result = child.wait() => result.map_err(|error| ExecError {
                message: format!("failed to wait for `{}`: {error}", cmd.command),
            })?,
            _ = ctx.cancel.cancelled() => terminate(&mut child).await?,
        };

        // The child has exited (or been reaped after termination), so its
        // pipes are closing/closed; let the reader tasks drain whatever is
        // left before reporting the result.
        let _ = stdout_task.await;
        let _ = stderr_task.await;

        Ok(ExecResult {
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

fn build_command(cmd: &CommandSpec) -> Command {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&cmd.command);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("powershell");
        command.args(["-NoProfile", "-Command", &cmd.command]);
        command
    };

    command
        .current_dir(&cmd.cwd)
        .envs(cmd.env.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Reads `reader` to EOF, splitting on `\n` and stripping a `\r` that
/// immediately precedes it (the CRLF line endings `powershell` produces on
/// windows), sending one [`OutputLine`] per line. A final line with no
/// trailing newline is still emitted; lines of any length are supported
/// since the buffer grows as needed rather than being capped.
async fn stream_lines<R>(
    reader: R,
    stream: Stream,
    output: tokio::sync::mpsc::UnboundedSender<OutputLine>,
) where
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
        // If the receiver has been dropped, nobody is listening for output
        // anymore; keep draining the pipe regardless so the child is never
        // blocked writing into a full pipe while it still runs.
        let _ = output.send(OutputLine { stream, text });
    }
}

/// Terminates a still-running child after cancellation: send the
/// platform's graceful-stop request, wait up to [`GRACE_PERIOD`] for it to
/// exit on its own, then escalate to an unconditional kill.
///
/// On unix the graceful request is a real `SIGTERM`, distinct from the
/// forceful `SIGKILL` used on escalation. Windows has no equivalent of
/// `SIGTERM` for an arbitrary process — `TerminateProcess` (what
/// `Child::start_kill` issues) is unconditional — so both steps use the
/// same forceful termination there; the grace period is harmless but in
/// practice never has anything left to wait out.
async fn terminate(child: &mut Child) -> Result<ExitStatus, ExecError> {
    send_terminate_signal(child);

    match tokio::time::timeout(GRACE_PERIOD, child.wait()).await {
        Ok(result) => result.map_err(|error| ExecError {
            message: format!("failed to wait for cancelled child: {error}"),
        }),
        Err(_elapsed) => {
            child.kill().await.map_err(|error| ExecError {
                message: format!("failed to force-kill child after grace period: {error}"),
            })?;
            child.wait().await.map_err(|error| ExecError {
                message: format!("failed to wait for force-killed child: {error}"),
            })
        }
    }
}

#[cfg(unix)]
fn send_terminate_signal(child: &Child) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    if let Some(pid) = child.id() {
        // Best-effort: if the process already exited between us checking
        // and sending, `kill` returning an error (ESRCH) is fine — the
        // subsequent `wait` will observe the exit either way.
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
}

#[cfg(windows)]
fn send_terminate_signal(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    async fn collect(data: &[u8]) -> Vec<String> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        stream_lines(Cursor::new(data.to_vec()), Stream::Stdout, tx).await;
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
        // Only a `\r` immediately preceding the `\n` we split on is a line
        // terminator artifact; a `\r` elsewhere in the line is real content.
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
