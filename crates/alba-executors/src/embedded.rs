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

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<alba_shell::ShellOutputLine>();
        let forward = {
            let output = ctx.output.clone();
            tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    let stream = match line.stream {
                        alba_shell::ShellStream::Stdout => Stream::Stdout,
                        alba_shell::ShellStream::Stderr => Stream::Stderr,
                    };
                    let _ = output.send(OutputLine {
                        stream,
                        text: line.text,
                    });
                }
            })
        };

        let result = alba_shell::execute(
            &program,
            alba_shell::ShellEnv {
                env,
                cwd: cmd.cwd,
                output: tx,
                cancel: ctx.cancel,
            },
        )
        .await;
        // Dropping `tx` above (moved into `ShellEnv`) is what lets the
        // forwarding task's `recv` loop end; join it so every line reaches
        // `ctx.output` before this returns.
        let _ = forward.await;

        Ok(ExecResult {
            exit_code: result.exit_code,
        })
    }
}
