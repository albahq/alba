//! Spawns a resolved external binary on the streams a `CommandIo`
//! describes, honouring cancellation with a graceful-then-forceful
//! termination.
//!
//! Ported from `alba-executors::shell::SystemShellExecutor`: this crate
//! cannot depend on `alba-executors` (it must stay standalone), but the
//! process-spawning, line-streaming, and cancellation-escalation
//! behaviour is the same — just wired to a resolved external binary
//! instead of `sh -c`/`powershell -Command`.

use std::io::Write;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::io::{CommandIo, OutTarget};

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
/// `cwd`, on the three streams `io` describes — the run's output channel
/// line-by-line, a capture buffer, a redirected file, or a pipeline
/// neighbour — honouring `cancel` with a graceful-then-forceful
/// termination.
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
    io: CommandIo,
    cancel: &CancellationToken,
) -> i32 {
    let CommandIo {
        stdin,
        stdout,
        stderr,
    } = io;

    // A second handle on the command's stderr, kept only until the spawn
    // has been attempted: once `stderr` itself has become a `Stdio`
    // there is no writer left to report a spawn failure through. It is
    // dropped the moment the spawn succeeds — a lingering duplicate of a
    // pipe's write end would leave the next stage waiting for an EOF
    // that could never arrive.
    let error_sink = stderr.try_clone();

    let (stdout_stdio, stdout_sink) = stdout.into_stdio();
    let (stderr_stdio, stderr_sink) = stderr.into_stdio();

    let mut command = build_command(path, args, env, cwd);
    command
        .stdin(stdin.into_stdio())
        .stdout(stdout_stdio)
        .stderr(stderr_stdio);
    let spawned = command.spawn();
    // The `Command` still owns this process's copy of every handle it
    // was given, including the write end of a pipeline pipe and any
    // redirected file: dropping it here is what lets the next stage's
    // reader ever reach EOF. Holding it until the end of the function
    // would deadlock a downstream stage.
    drop(command);

    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => return report_spawn_failure(error_sink, path, &error),
    };
    drop(error_sink);

    // A sink comes back exactly when `into_stdio` asked for
    // `Stdio::piped()`, which is exactly when the child has a handle to
    // take, so these two always agree.
    let mut stdout_task = stdout_sink.map(|sink| {
        let handle = child
            .stdout
            .take()
            .expect("child spawned with piped stdout");
        tokio::spawn(sink.forward(handle))
    });
    let mut stderr_task = stderr_sink.map(|sink| {
        let handle = child
            .stderr
            .take()
            .expect("child spawned with piped stderr");
        tokio::spawn(sink.forward(handle))
    });

    let status: Option<ExitStatus> = tokio::select! {
        result = child.wait() => result.ok(),
        _ = cancel.cancelled() => terminate(&mut child).await,
    };

    // The child has exited (or been reaped after termination), so its
    // pipes are closing/closed; let the forwarding tasks drain whatever
    // is left, but only for up to DRAIN_PERIOD total — see its doc
    // comment for why this must be bounded rather than an unconditional
    // await. Both tasks share a single DRAIN_PERIOD budget (joined
    // concurrently, not one after another): awaiting them sequentially
    // would double the worst-case wait to `2 * DRAIN_PERIOD` for no
    // benefit, since both pipes can stall for the same reason (a
    // surviving descendant) at the same time.
    //
    // Aborting on timeout is what makes that bound real rather than
    // cosmetic: it drops the task, closing this process's read end of a
    // pipe a stray descendant is still holding open. Without it the
    // reader would live on past this call — and, because dropping a
    // runtime waits for its tasks, could keep the whole host process
    // from exiting.
    let drain = async {
        tokio::join!(join(stdout_task.as_mut()), join(stderr_task.as_mut()));
    };
    if tokio::time::timeout(DRAIN_PERIOD, drain).await.is_err() {
        abort(stdout_task.as_ref());
        abort(stderr_task.as_ref());
    }

    status.and_then(|s| s.code()).unwrap_or(-1)
}

/// Waits for a forwarding task, if this target needed one at all (a file
/// or a pipeline neighbour is handed straight to the child, with nothing
/// in between to wait for).
async fn join(task: Option<&mut JoinHandle<()>>) {
    if let Some(task) = task {
        let _ = task.await;
    }
}

fn abort(task: Option<&JoinHandle<()>>) {
    if let Some(task) = task {
        task.abort();
    }
}

fn report_spawn_failure(
    sink: std::io::Result<OutTarget>,
    path: &Path,
    error: &std::io::Error,
) -> i32 {
    if let Ok(sink) = sink {
        let mut writer = sink.writer();
        let _ = writeln!(
            writer,
            "alba-shell: failed to spawn {}: {error}",
            path.display()
        );
    }
    126
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

    // Streams are left to the caller: `run_external` sets them from the
    // `CommandIo` it was handed.
    command
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned());
    command
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
