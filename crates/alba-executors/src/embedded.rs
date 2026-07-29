//! [`EmbeddedShellExecutor`]: runs a [`CommandSpec`] through `alba-shell`,
//! Alba's own POSIX-like interpreter, instead of spawning a host shell.
//!
//! This is the default executor (`ExecutorKind::Shell`): every beam that
//! does not declare `executor system_shell` runs here.

use crate::{CommandSpec, ExecContext, ExecError, ExecResult, Executor, OutputLine, Stream};

/// Runs commands by parsing and interpreting them with `alba-shell`
/// directly, in-process: no `sh`/`powershell` child is spawned for the
/// shell itself (external programs a command invokes are still spawned as
/// child processes by `alba-shell`).
pub struct EmbeddedShellExecutor;

#[async_trait::async_trait]
impl Executor for EmbeddedShellExecutor {
    async fn execute(&self, cmd: CommandSpec, ctx: ExecContext) -> Result<ExecResult, ExecError> {
        // A parse failure must never spawn anything: it becomes a beam
        // failure carrying the rendered diagnostic (message, source
        // excerpt, caret, and the `executor system_shell` suggestion) as
        // its output, exactly like any other command failure the engine
        // reports.
        let program = alba_shell::parse(&cmd.command).map_err(|error| ExecError {
            message: error.render(&cmd.command),
        })?;

        // "Extends and overrides", per `CommandSpec::env`'s documented
        // contract: start from the executing process's own environment,
        // then let the beam's env win on matching names.
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        for (name, value) in &cmd.env {
            match env.iter_mut().find(|(existing, _)| existing == name) {
                Some(slot) => slot.1 = value.clone(),
                None => env.push((name.clone(), value.clone())),
            }
        }

        let ExecContext { output, cancel } = ctx;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<alba_shell::ShellOutputLine>();
        let shell = alba_shell::execute(
            &program,
            alba_shell::ShellEnv {
                env,
                cwd: cmd.cwd,
                output: tx,
                cancel,
            },
        );
        tokio::pin!(shell);

        // Lines are forwarded *while* the run is in flight, and the run
        // ending is what ends the forwarding. Waiting instead for the
        // channel to close would be waiting for the last sender clone to
        // drop, and a cancelled pipeline can leave a stage detached
        // holding one: a builtin stage runs on a detached thread, which is
        // not waited on by the runtime, so "the channel closed" can trail
        // "the run finished" by however long that stage takes. Awaiting it
        // here would hand the whole cancellation delay straight back to the
        // caller, which is precisely what `alba_shell::execute` returning
        // promptly is meant to prevent.
        let result = loop {
            tokio::select! {
                Some(line) = rx.recv() => forward(&output, line),
                result = &mut shell => break result,
            }
        };

        // The run is over, but lines it already emitted may still be
        // queued ahead of this point. Take those, in order, before
        // returning: nothing the run produced is lost, and anything a
        // detached stage sends afterwards belongs to no run at all.
        while let Ok(line) = rx.try_recv() {
            forward(&output, line);
        }

        Ok(ExecResult {
            exit_code: result.exit_code,
        })
    }
}

/// Relays one shell output line to the executor's own output channel. A
/// failed send means the receiver is gone, which is never a reason to
/// interrupt a running command.
fn forward(
    output: &tokio::sync::mpsc::UnboundedSender<OutputLine>,
    line: alba_shell::ShellOutputLine,
) {
    let stream = match line.stream {
        alba_shell::ShellStream::Stdout => Stream::Stdout,
        alba_shell::ShellStream::Stderr => Stream::Stderr,
    };
    let _ = output.send(OutputLine {
        stream,
        text: line.text,
    });
}
