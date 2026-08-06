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

use std::path::{Path, PathBuf};
use std::time::Duration;

use alba_executors::protocol::PROTOCOL_VERSION;
use alba_executors::{
    BeamContext, CommandSpec, ExecContext, ExecResult, ExecSession, Executor, OutputLine,
    PluginExecutor,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::args::PluginCommand;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

/// The protocol's own handshake timeout. Mirrored here, rather than read
/// off `PluginExecutor::new`'s default, because the crate keeps that
/// default private — and the cancellation check below needs to know the
/// exact grace it is bounding its own tolerance against, not a guess that
/// could silently drift from the real one.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The protocol's own cancellation grace, mirrored for the same reason as
/// [`HANDSHAKE_TIMEOUT`].
const GRACE: Duration = Duration::from_secs(5);

/// How long after sending the cancellation check's command this check
/// waits before asking the plugin to cancel it.
const CANCEL_AFTER: Duration = Duration::from_millis(100);

/// Slack added on top of [`GRACE`] when bounding the cancel check, so
/// ordinary scheduling jitter is never mistaken for a hang.
const CANCEL_TOLERANCE: Duration = Duration::from_secs(1);

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
        options: serde_json::Value::Null,
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
/// Takes `session` by value and never closes it: this check is its only
/// use, and letting it drop here — rather than closing it — is deliberate,
/// see the module doc comment for why only the cancellation check's fresh
/// session is closed explicitly. `kill_on_drop` on the underlying child
/// (see `alba_executors::PluginExecutor`) cleans this process up the
/// moment `session` drops, whether that is here on a normal return or via
/// unwinding if something above panics.
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

    match tokio::time::timeout(EXECUTE_TIMEOUT, session.execute(cmd, ctx)).await {
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
async fn cancel_and_close_check(
    binary: &Path,
    cancel_command: &str,
    out: &mut LineSink<std::io::Stdout>,
) -> bool {
    let executor = PluginExecutor::with_timeouts(binary.to_path_buf(), HANDSHAKE_TIMEOUT, GRACE);
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
    // own `GRACE`-bounded teardown, which already returns `Ok` (never
    // hangs) whether the plugin answers `cancel` promptly or has to be
    // killed once `GRACE` elapses — see `PluginExecutor::execute`'s own
    // handling. This is the safety net for the one path that isn't
    // already covered by that: something in the host's own kill-and-reap
    // sequence itself getting stuck.
    let bound = GRACE + CANCEL_TOLERANCE;
    let cancel_conforms = match tokio::time::timeout(bound, session.execute(cmd, ctx)).await {
        Ok(Ok(_result)) => {
            out.line("\u{2713} cancel");
            true
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
            // completing its own teardown; nothing here has any more
            // handle on the process than `session` itself. Dropping it in
            // turn is what stops it: `kill_on_drop` on the underlying
            // child (see `alba_executors::PluginExecutor`) reaches it even
            // though this path never calls `close`.
            drop(session);
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
