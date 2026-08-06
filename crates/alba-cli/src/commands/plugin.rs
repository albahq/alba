//! `alba plugin check <BINARY>`: drives a plugin binary through protocol v1
//! end to end and reports, check by check, whether it conforms. Dispatched
//! in `main.rs` before the Beamfile loads — see the `commands` module doc
//! comment — since a conformance check has no beam to run and needs no
//! Beamfile at all.
//!
//! Four checks run in order, each printing one line through
//! [`LineSink::stdout`]:
//!
//! 1. **handshake** — open a session against the binary.
//! 2. **execute** — run `--command` on that same session.
//! 3. **cancel** — on a **fresh** session (a fresh process, not the one
//!    `execute` just used): run `--cancel-command`, cancelling it 100ms in.
//!    A session that just answered a cancellation is left in an
//!    indeterminate state, so it must never be reused for anything after.
//! 4. **close** — close that same fresh session.
//!
//! If the handshake itself fails, the other three are skipped rather than
//! attempted: they would only reopen the same unresponsive binary and pay
//! its handshake timeout a second time for a foregone conclusion.
//!
//! ## Every session this drives is either closed or killed
//!
//! A session this module gives up on early — a protocol violation
//! mid-`execute`, the execute check's own timeout, the cancel check's
//! safety-net timeout — is never simply dropped. `Drop` alone reaches only
//! a plugin's immediate process (see `alba_executors::PluginExecutor`'s own
//! `kill_on_drop`), not whatever that process may itself have spawned to
//! run the checked command; `ExecSession::kill` is the crate's own
//! process-group-aware teardown, the same one every internal give-up path
//! in `PluginExecutor` already uses, and every give-up path here uses it
//! too rather than trusting a bare drop to do the same job.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alba_executors::protocol::PROTOCOL_VERSION;
use alba_executors::{
    BeamContext, CommandSpec, ExecContext, ExecResult, ExecSession, Executor, OutputLine,
    PluginExecutor,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::args::PluginCommand;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

/// How long after sending the cancellation check's command this check
/// waits before asking the plugin to cancel it.
const CANCEL_AFTER: Duration = Duration::from_millis(100);

/// Slack added on top of [`PluginExecutor::DEFAULT_GRACE`] when bounding
/// the cancel check's own outer wait, so ordinary scheduling jitter is
/// never mistaken for a hang in the host's own kill-and-reap sequence (see
/// [`cancel_and_close_check`]'s doc comment).
const CANCEL_TOLERANCE: Duration = Duration::from_secs(1);

/// The fraction of [`PluginExecutor::DEFAULT_GRACE`] an answer to `cancel`
/// must land inside to count as prompt, rather than as the host's own
/// grace-triggered kill masquerading as one — see
/// [`cancel_and_close_check`]'s doc comment for why elapsed time is the
/// only signal available at all. Chosen, not measured: wide enough that a
/// real plugin answering within a couple hundred milliseconds has ample
/// margin even on a loaded machine, tight enough that a plugin whose
/// answer only arrives once the grace-driven kill fires (necessarily
/// close to 100% of the grace) cannot be mistaken for a prompt one.
const CANCEL_PROMPT_FRACTION: f32 = 0.8;

/// How long the execute check waits for an `exit` message before giving up
/// — generous on purpose: this check is timing the plugin's own command,
/// not the protocol round trip.
const EXECUTE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn run(command: &PluginCommand) -> i32 {
    let PluginCommand::Check {
        binary,
        command,
        cancel_command,
    } = command;

    // Mirrors `commands::run::run`'s own runtime: a multi-threaded runtime
    // with every driver enabled, which `Runtime::new()` builds directly —
    // the plugin process this drives, and the cancellation timer racing
    // it, both need it.
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            LineSink::stderr().line(&format!("cannot start the async runtime: {error}"));
            return EXIT_ALBA_ERROR;
        }
    };

    runtime.block_on(check(binary, command, cancel_command))
}

async fn check(binary: &Path, command: &str, cancel_command: &str) -> i32 {
    let mut out = LineSink::stdout();

    if !binary.is_file() {
        out.line(&format!(
            "cannot check `{}`: no such file",
            binary.display()
        ));
        return EXIT_ALBA_ERROR;
    }

    let (session, tx, mut rx) = match open_session(PluginExecutor::new(binary.to_path_buf())).await
    {
        Ok(opened) => {
            out.line("\u{2713} handshake");
            opened
        }
        Err(message) => {
            out.line(&format!("\u{2717} handshake: {message}"));
            out.line("not conformant");
            return 1;
        }
    };

    let mut conformant = execute_check(session, command, tx, &mut rx, &mut out).await;
    conformant &= cancel_and_close_check(binary, cancel_command, &mut out).await;

    if conformant {
        out.line(&format!("conformant: protocol v{PROTOCOL_VERSION}"));
        0
    } else {
        out.line("not conformant");
        1
    }
}

/// Opens one session against `executor`, returning the output channel's
/// two ends alongside it: `open`'s `BeamContext` needs a sender to relay
/// the plugin's stderr for the whole session's lifetime, and the caller
/// keeps the receiver to drain whatever a later `execute` on this same
/// session produces.
async fn open_session(
    executor: PluginExecutor,
) -> Result<
    (
        Box<dyn ExecSession>,
        UnboundedSender<OutputLine>,
        UnboundedReceiver<OutputLine>,
    ),
    String,
> {
    let (tx, rx) = unbounded_channel();
    let context = BeamContext {
        beam: "plugin-check".to_string(),
        dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        // An empty object, never `null`: `docs/plugin-protocol.md` (see
        // `open`'s fields) guarantees every beam sends `options` as `{}`
        // when it declares none, and the engine itself never sends `null`
        // (see `alba-engine`'s `executor_options`) — a plugin coding
        // against that guarantee (`options.as_object().unwrap()`, or
        // deserializing into a struct) must see the same shape here.
        options: serde_json::json!({}),
        output: tx.clone(),
        cancel: CancellationToken::new(),
    };
    let session = executor
        .open(context)
        .await
        .map_err(|error| error.to_string())?;
    Ok((session, tx, rx))
}

/// The execute check: runs `command` on the session the handshake check
/// just opened. Any `Ok(ExecResult { .. })` conforms — the plugin's exit
/// code is its own business; protocol conformance is only that an `exit`
/// message arrived at all.
///
/// Takes `session` by value and always ends it with [`ExecSession::kill`],
/// never `close`: this check is the session's only use — see the module
/// doc comment for why only the cancellation check's fresh session is
/// closed — and `kill` is used unconditionally, on every outcome
/// (success, a protocol violation, this check's own timeout), rather than
/// just letting `session` drop. A clean `exit` message means the checked
/// command itself has already finished, but says nothing about a
/// descendant it may have left behind; the timeout and error arms can
/// abandon the command mid-flight outright. `kill` reaches a spawned
/// plugin's whole process group whenever one is still alive to reach —
/// which a bare drop cannot, see the module doc comment — but not when
/// the plugin had already exited on its own by the time this runs (an
/// EOF-driven `Err` from `execute`, or the plugin exiting between the
/// timeout firing and this call): `PluginExecutor::kill`'s own doc
/// comment covers why there is nothing left to signal on that path, and
/// why any descendant the plugin left behind then survives unkilled.
async fn execute_check(
    mut session: Box<dyn ExecSession>,
    command: &str,
    tx: UnboundedSender<OutputLine>,
    rx: &mut UnboundedReceiver<OutputLine>,
    out: &mut LineSink<std::io::Stdout>,
) -> bool {
    let cmd = CommandSpec {
        command: command.to_string(),
        env: Vec::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let ctx = ExecContext {
        output: tx,
        cancel: CancellationToken::new(),
    };

    let outcome = tokio::time::timeout(EXECUTE_TIMEOUT, session.execute(cmd, ctx)).await;
    session.kill().await;

    match outcome {
        Ok(Ok(ExecResult { exit_code })) => {
            // `execute` only returns once every `output` line the command
            // produced has already been pushed onto this same channel —
            // it awaits the final `exit` message in the same sequential
            // loop that forwards each `output` one first — so draining
            // with `try_recv` here cannot race a line still in flight.
            let mut lines = 0usize;
            while rx.try_recv().is_ok() {
                lines += 1;
            }
            let plural = if lines == 1 { "" } else { "s" };
            out.line(&format!(
                "\u{2713} execute: exit code {exit_code}, {lines} output line{plural}"
            ));
            true
        }
        Ok(Err(error)) => {
            out.line(&format!("\u{2717} execute: {error}"));
            false
        }
        Err(_elapsed) => {
            out.line(&format!(
                "\u{2717} execute: no `exit` message within {EXECUTE_TIMEOUT:?}"
            ));
            false
        }
    }
}

/// The cancellation and close checks, run together because they share the
/// one fresh session: opening it is [`open_session`] against a brand new
/// process, never the one [`execute_check`] just used — see the module
/// doc comment for why cancelling forbids reuse.
///
/// ## Why elapsed time, not just `Ok`/`Err`, decides the cancel check
///
/// `PluginExecutor::execute` absorbs a plugin that never answers `cancel`
/// at all: once its own grace elapses it force-kills the plugin and
/// returns `Ok(ExecResult { .. })` regardless — the same `Ok` a plugin that
/// answered promptly would return. So `Ok`/`Err` alone cannot tell a
/// conformant plugin apart from one that only appears to answer because
/// the host had to kill it; the elapsed time can, and is measured for
/// exactly that.
///
/// Two different elapsed measurements matter here, not one:
///
/// - `total_elapsed`, timed from just before `execute` is called, guards
///   against `--cancel-command` finishing on its own before `cancel` was
///   even sent (100ms in): a command that short makes every later
///   measurement meaningless, since nothing about "how it answered
///   `cancel`" was ever actually exercised. This is checked first and
///   fails the check outright.
/// - `answered_in`, `total_elapsed` with [`CANCEL_AFTER`] subtracted back
///   out, is time *since the plugin was actually asked to stop* — what
///   [`CANCEL_PROMPT_FRACTION`] of `grace` and the failure message below
///   are actually about. Grading against `total_elapsed` instead would
///   silently fold `CANCEL_AFTER` into the budget a slow-but-honest answer
///   gets, and would describe a plugin that took, say, 3.95 seconds to
///   answer `cancel` as one that "had to be killed" at the 5-second grace,
///   which is false on both counts.
async fn cancel_and_close_check(
    binary: &Path,
    cancel_command: &str,
    out: &mut LineSink<std::io::Stdout>,
) -> bool {
    let executor = PluginExecutor::with_timeouts(
        binary.to_path_buf(),
        PluginExecutor::DEFAULT_HANDSHAKE_TIMEOUT,
        PluginExecutor::DEFAULT_GRACE,
    );
    let (mut session, tx, _rx) = match open_session(executor).await {
        Ok(opened) => opened,
        Err(message) => {
            out.line(&format!(
                "\u{2717} cancel: cannot open a fresh session — {message}"
            ));
            out.line("\u{2717} close: skipped — no session to close");
            return false;
        }
    };

    let cancel = CancellationToken::new();
    let fire = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(CANCEL_AFTER).await;
        fire.cancel();
    });

    let cmd = CommandSpec {
        command: cancel_command.to_string(),
        env: Vec::new(),
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let ctx = ExecContext { output: tx, cancel };

    // The bound this check itself enforces on top of `PluginExecutor`'s
    // own `DEFAULT_GRACE`-bounded teardown, which already returns `Ok`
    // (never hangs) whether the plugin answers `cancel` promptly or has to
    // be killed once the grace elapses — see `PluginExecutor::execute`'s
    // own handling, and the doc comment above on why that `Ok` alone is
    // not enough to grade this check. This bound is the safety net for the
    // one path that isn't already covered by that: something in the
    // host's own kill-and-reap sequence itself getting stuck.
    let grace = PluginExecutor::DEFAULT_GRACE;
    let bound = grace + CANCEL_TOLERANCE;
    let started = Instant::now();
    let outcome = tokio::time::timeout(bound, session.execute(cmd, ctx)).await;
    let total_elapsed = started.elapsed();
    // See the doc comment above: time since `cancel` was actually sent,
    // not since `execute` was called. Saturating because a `total_elapsed`
    // below `CANCEL_AFTER` — caught by the check below before this value
    // is ever used to grade anything — would otherwise underflow.
    let answered_in = total_elapsed.saturating_sub(CANCEL_AFTER);

    let cancel_conforms = match outcome {
        Ok(Ok(_result)) if total_elapsed < CANCEL_AFTER => {
            out.line(&format!(
                "\u{2717} cancel: `--cancel-command` finished in {total_elapsed:?}, before \
                 `cancel` was even sent at {CANCEL_AFTER:?} — pick a command that runs longer"
            ));
            false
        }
        Ok(Ok(_result)) if answered_in < grace.mul_f32(CANCEL_PROMPT_FRACTION) => {
            out.line(&format!("\u{2713} cancel (answered in {answered_in:?})"));
            true
        }
        Ok(Ok(_result)) => {
            out.line(&format!(
                "\u{2717} cancel: no answer within the {grace:?} grace ({answered_in:?} after \
                 cancel); the host had to kill the plugin"
            ));
            false
        }
        Ok(Err(error)) => {
            out.line(&format!("\u{2717} cancel: {error}"));
            false
        }
        Err(_elapsed) => {
            out.line(&format!(
                "\u{2717} cancel: no response within {bound:?} of asking it to stop"
            ));
            out.line("\u{2717} close: skipped — the cancel check did not return");
            // The in-flight `execute` future was just dropped without
            // completing its own teardown; `kill` reaches the plugin's
            // whole process group directly (see the module doc comment)
            // rather than trusting `Drop` to reach the same thing.
            session.kill().await;
            return false;
        }
    };

    match session.close().await {
        Ok(()) => {
            out.line("\u{2713} close");
            cancel_conforms
        }
        Err(error) => {
            out.line(&format!("\u{2717} close: {error}"));
            false
        }
    }
}
