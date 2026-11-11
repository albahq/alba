//! Container plumbing shared by the executors that drive a long-lived
//! container through the `docker` CLI: naming, `docker exec` with its
//! cancellation ladder and output draining, and removal.

use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::process::Command;

use crate::shell::stream_lines;
use crate::{CommandSpec, ExecContext, ExecError, ExecResult, Stream};

/// How long a cancelled command's whole teardown sequence — asking the
/// daemon to stop the container, removing it, and reaping the local
/// `docker` process — may take before escalating to a forceful kill of
/// that local process. This bounds the *entire* sequence, not just the
/// final wait: a slow or unreachable daemon must not be able to hold a
/// cancelled beam past the grace period this advertises.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the stdout/stderr reader tasks to observe EOF
/// after a `docker exec` child has exited, before giving up on further
/// output and returning anyway. Mirrors `shell.rs`'s `DRAIN_PERIOD`: a
/// command that backgrounds a descendant inside the container could
/// otherwise hold the pipe open and block `execute()` forever waiting for
/// an EOF that never comes.
pub(crate) const DRAIN_PERIOD: Duration = Duration::from_secs(2);

/// Maps a spawn failure of the `docker` binary itself (as opposed to a
/// non-zero exit from a `docker` subcommand) to a hint that the executor
/// needs `docker` on the `PATH`. `requirement` finishes the sentence and
/// names the executor that needs it.
pub(crate) fn spawn_error(error: std::io::Error, requirement: &str) -> ExecError {
    ExecError {
        message: format!("cannot run `docker`: {error} — {requirement}"),
    }
}

/// Runs a `docker` subcommand to completion, discarding its output; used
/// for cleanup steps (`stop`, `rm -f`) whose own failure is not worth
/// surfacing since the caller has already decided the container is going
/// away regardless.
pub(crate) async fn run_quiet(args: &[&str]) {
    if let Ok(mut child) = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        let _ = child.wait().await;
    }
}

/// A unique, docker-legal name for the container backing one beam session:
/// `alba-<sanitized beam>-<pid>-<counter>`. The pid and a process-local
/// counter together make names unique across concurrent Alba processes and
/// concurrent beams within one process (docker container names must be
/// unique on the host).
pub(crate) fn container_name(beam: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sanitized: String = beam
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("alba-{sanitized}-{}-{counter}", std::process::id())
}

/// Builds the argument vector for `docker exec`, running one already
/// rendered command line inside the session's container via `sh -c`.
pub(crate) fn exec_args(
    container: &str,
    cwd: &str,
    env: &[(String, String)],
    command: &str,
) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "-w".to_string(), cwd.to_string()];
    for (key, value) in env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(container.to_string());
    args.push("sh".to_string());
    args.push("-c".to_string());
    args.push(command.to_string());
    args
}

/// Runs one command inside `container` through `docker exec`, streaming its
/// output to `ctx`, and returns its exit code. `cwd` is the in-container
/// working directory. Cancelling stops and removes the container.
/// `requirement` is handed to [`spawn_error`].
pub(crate) async fn exec(
    container: &str,
    cwd: &str,
    cmd: &CommandSpec,
    ctx: ExecContext,
    requirement: &str,
) -> Result<ExecResult, ExecError> {
    let args = exec_args(container, cwd, &cmd.env, &cmd.command);
    let mut child = Command::new("docker")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_error(error, requirement))?;

    let stdout = child
        .stdout
        .take()
        .expect("child spawned with piped stdout");
    let stderr = child
        .stderr
        .take()
        .expect("child spawned with piped stderr");

    let mut stdout_task = tokio::spawn(stream_lines(stdout, Stream::Stdout, ctx.output.clone()));
    let mut stderr_task = tokio::spawn(stream_lines(stderr, Stream::Stderr, ctx.output.clone()));

    let status_result: Result<ExitStatus, ExecError> = tokio::select! {
        result = child.wait() => result.map_err(|error| ExecError {
            message: format!("failed to wait for `docker exec`: {error}"),
        }),
        _ = ctx.cancel.cancelled() => {
            // Ask the daemon to stop the container, then make sure the
            // local `docker exec` process is gone. The whole sequence
            // — stop, rm, and reaping the child — is bounded by
            // STOP_GRACE so a slow or unreachable daemon cannot hold
            // the beam past the grace period it advertises; only the
            // fallback kill is unbounded, and it targets our own local
            // process, which cannot itself hang on the daemon.
            let teardown = async {
                run_quiet(&["stop", "--time", &STOP_GRACE.as_secs().to_string(), container]).await;
                run_quiet(&["rm", "-f", container]).await;
                let _ = child.wait().await;
            };
            if tokio::time::timeout(STOP_GRACE, teardown).await.is_err() {
                let _ = child.kill().await;
            }
            child.wait().await.map_err(|error| ExecError {
                message: format!("failed to wait for cancelled `docker exec`: {error}"),
            })
        }
    };

    // On error, nobody is going to read `ctx.output` after `execute`
    // returns; abort the reader tasks instead of leaving them
    // detached and still writing to a channel side nothing drains.
    let status = match status_result {
        Ok(status) => status,
        Err(error) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(error);
        }
    };

    // The child has exited, so its pipes are closing/closed; let the
    // reader tasks drain what's left, bounded by DRAIN_PERIOD — see
    // its doc comment, mirroring `shell.rs`'s identical reasoning: an
    // unconditional join could hang forever on a surviving descendant
    // inside the container still holding a pipe open.
    let drain = async {
        let _ = tokio::join!(&mut stdout_task, &mut stderr_task);
    };
    if tokio::time::timeout(DRAIN_PERIOD, drain).await.is_err() {
        stdout_task.abort();
        stderr_task.abort();
    }

    Ok(ExecResult {
        exit_code: status.code().unwrap_or(-1),
    })
}

/// Removes `container` with `docker rm -f`. A container that is already gone
/// is not an error. `requirement` is handed to [`spawn_error`].
pub(crate) async fn remove(container: &str, requirement: &str) -> Result<(), ExecError> {
    let output = Command::new("docker")
        .args(["rm", "-f", container])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| spawn_error(error, requirement))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such container") {
        // A cancelled `execute` already stopped and removed this
        // container (see its teardown sequence); `close` still runs
        // unconditionally afterwards, so this is the expected,
        // truthful outcome, not a failure the user needs to see.
        return Ok(());
    }
    let code = output.status.code().unwrap_or(-1);
    Err(ExecError {
        message: format!("docker rm exited with code {code}: {}", stderr.trim()),
    })
}

/// The fire-and-forget form of [`remove`], for callers with no use for its
/// diagnostic.
pub(crate) async fn force_remove(container: &str) {
    // The same removal `close` performs — `docker rm -f` reaches
    // everything running inside the container, which is the whole
    // process group concern `kill`'s contract cares about — but
    // fire-and-forget: a caller reaching for `kill` instead of `close`
    // has no use for the diagnostic `close` returns on failure.
    let _ = Command::new("docker")
        .args(["rm", "-f", container])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_are_sanitized_and_unique() {
        let a = container_name("api:build");
        let b = container_name("api:build");
        assert!(a.starts_with("alba-api-build-"));
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        );
    }

    #[test]
    fn exec_args_carry_workdir_and_env() {
        let args = exec_args("cid", "/proj/api", &[("K".into(), "V".into())], "echo hi");
        assert_eq!(
            args,
            vec![
                "exec",
                "-w",
                "/proj/api",
                "-e",
                "K=V",
                "cid",
                "sh",
                "-c",
                "echo hi",
            ]
        );
    }
}
