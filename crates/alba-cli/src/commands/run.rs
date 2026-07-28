//! `alba run <beam> [params...]`: schedule a beam and everything it needs,
//! render the event stream, and report the run's exit code.
//!
//! ## Shape
//!
//! Three concurrent pieces, all owned by one `block_on`:
//!
//! - the engine's [`alba_engine::run`] future, awaited here;
//! - a task consuming the event channel and feeding a [`Renderer`], so
//!   output is streamed while beams are still running rather than buffered
//!   until the run ends;
//! - a task watching for interrupts (see [`watch_interrupts`]).
//!
//! The consumer's channel closes on its own when the engine returns (the
//! engine holds the last sender), so joining it afterwards cannot outlive
//! the run.

use std::io::IsTerminal;
use std::num::NonZeroUsize;
use std::sync::Arc;

use alba_core::{BeamId, Project, SourceMap};
use alba_engine::{EngineError, RunEvent, RunOptions};
use alba_executors::SystemShellExecutor;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::args::{LogFormat, OutputStyle, RunFlags};
use crate::exit::{EXIT_ALBA_ERROR, EXIT_INTERRUPTED};
use crate::render::{GroupedRenderer, InterleavedRenderer, JsonRenderer, Renderer};

/// Runs `target` and returns the process exit code.
///
/// Loading already happened in `main.rs` (see the `commands` module doc
/// comment); `sources` is carried in only to render an [`EngineError`]
/// that points at a span in a Beamfile — an unknown target, or a `run`
/// template that fails at schedule time.
pub fn run(
    project: &Project,
    sources: &SourceMap,
    target: &BeamId,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    // `enable_all` is required, not incidental: the shell executor drives
    // child processes and the interrupt watcher waits on signals, both of
    // which need the I/O and signal drivers.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return EXIT_ALBA_ERROR;
        }
    };

    runtime.block_on(execute(project, sources, target, params, flags))
}

async fn execute(
    project: &Project,
    sources: &SourceMap,
    target: &BeamId,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
    };

    let (events, incoming) = unbounded_channel();
    let cancel = CancellationToken::new();

    tokio::spawn(watch_interrupts(cancel.clone()));
    let consumer = tokio::spawn(consume(incoming, renderer(flags), cancel.clone()));

    let result = alba_engine::run(
        project,
        target,
        options,
        Arc::new(SystemShellExecutor),
        events,
        cancel,
    )
    .await;

    // Every remaining event is already queued (the channel is unbounded),
    // and the engine dropped the last sender by returning — so this joins
    // once the renderer has drained the run, and cannot hang.
    let _ = consumer.await;

    match result {
        Ok(summary) => summary.exit_code(),
        Err(error) => {
            eprint!("{}", render_engine_error(error, sources));
            EXIT_ALBA_ERROR
        }
    }
}

/// Feeds every event to `renderer` until the run ends, and announces the
/// first cancellation.
///
/// `cancelling...` is not a [`RunEvent`] — nothing in the engine emits it —
/// so it is printed here, on stderr, rather than pushed through
/// [`Renderer::handle`]. That keeps stdout's format contract intact for
/// all three renderers (in particular, it does not put a non-JSON line in
/// the middle of `--log-format json`'s stream), and it is what the user
/// needs to see: from the moment the token fires, stopping the running
/// commands can take the executor's grace period plus its output drain, so
/// without this line a first Ctrl-C looks like it did nothing.
async fn consume(
    mut incoming: UnboundedReceiver<RunEvent>,
    mut renderer: Box<dyn Renderer>,
    cancel: CancellationToken,
) {
    let mut announced = false;
    loop {
        tokio::select! {
            event = incoming.recv() => match event {
                Some(event) => renderer.handle(&event),
                None => break,
            },
            () = cancel.cancelled(), if !announced => {
                announced = true;
                eprintln!("cancelling...");
            }
        }
    }
}

/// Turns Ctrl-C into a cancellation, then into an abort.
///
/// This has to exist: `alba-executors` puts every command in its own
/// process group so that cancelling a beam kills the whole command tree,
/// and the necessary consequence is that a terminal Ctrl-C no longer
/// reaches those children. Alba is the only thing left that can stop them,
/// so without this watcher a Ctrl-C would kill `alba` and orphan every
/// running command.
///
/// The first signal cancels the run's token, which terminates the running
/// commands and lets the run finish reporting normally. Because that is
/// bounded but not instant, a second signal gives up on the orderly path
/// and leaves immediately with `130` — the conventional `128 + SIGINT`
/// code a shell reports for an interrupted command. That is deliberately
/// outside Alba's 0/1/2 vocabulary: those three describe how a *run* ended,
/// and a run that was abandoned mid-flight has no such answer to give.
/// Commands still alive at that point are left running, which is the
/// user's explicit second-Ctrl-C request to stop waiting.
async fn watch_interrupts(cancel: CancellationToken) {
    // An `Err` means no signal handler could be installed at all; there is
    // nothing to fall back to, so the watcher simply stands down.
    if tokio::signal::ctrl_c().await.is_err() {
        return;
    }
    cancel.cancel();

    if tokio::signal::ctrl_c().await.is_err() {
        return;
    }
    eprintln!("aborting");
    std::process::exit(EXIT_INTERRUPTED);
}

/// How many beams may run at once.
///
/// `--jobs 0` never reaches here: it is rejected during argument parsing
/// (see [`RunFlags::jobs`]), because a run with no slots is not a run and
/// silently reading it as `1` would hide a typo. When the flag is omitted
/// and the platform cannot report its parallelism, this falls back to `1`:
/// a sequential run is slow but always correct, whereas guessing a number
/// would oversubscribe a machine Alba knows nothing about. A requested
/// value beyond `usize` (only reachable on a 32-bit target) is clamped
/// rather than rejected — it already means "more than this machine will
/// ever run at once".
fn jobs(requested: Option<u64>) -> usize {
    match requested {
        Some(requested) => usize::try_from(requested).unwrap_or(usize::MAX),
        None => std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
    }
}

/// The renderer the flags select. `--output` only applies to
/// `--log-format text`: the JSON stream has one shape by definition.
fn renderer(flags: &RunFlags) -> Box<dyn Renderer> {
    match flags.log_format {
        LogFormat::Json => Box::new(JsonRenderer::new()),
        LogFormat::Text => match flags.output.unwrap_or_else(default_output) {
            OutputStyle::Interleaved => Box::new(InterleavedRenderer::new(crate::color_enabled())),
            OutputStyle::Grouped => Box::new(GroupedRenderer::new()),
        },
    }
}

/// Interleaved on a terminal (watching a run happen), grouped otherwise (a
/// log file or CI record, read after the fact). Only the TTY question is
/// asked here — `NO_COLOR` says nothing about which layout to use, which
/// is why this is not [`crate::color_enabled`].
fn default_output() -> OutputStyle {
    if std::io::stdout().is_terminal() {
        OutputStyle::Interleaved
    } else {
        OutputStyle::Grouped
    }
}

/// Renders an [`EngineError`] for stderr.
///
/// A `Core` error carries a span and a source file, so it gets the same
/// caret-and-help diagnostic a load failure does. The other variants are
/// plain sentences with nowhere in particular to point.
fn render_engine_error(error: EngineError, sources: &SourceMap) -> String {
    match error {
        EngineError::Core(error) => crate::render_core_error(error, sources),
        other => format!("{other}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_jobs_value_wins_over_the_platform_default() {
        assert_eq!(jobs(Some(3)), 3);
    }

    #[test]
    fn an_omitted_jobs_value_is_at_least_one() {
        assert!(jobs(None) >= 1);
    }
}
