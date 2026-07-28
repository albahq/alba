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
use std::path::Path;
use std::sync::Arc;

use alba_core::{BeamId, Project, SourceMap};
use alba_engine::{CacheOptions, EngineError, RunEvent, RunOptions, RunSummary};
use alba_executors::SystemShellExecutor;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::args::{LogFormat, OutputStyle, RunFlags};
use crate::exit::{EXIT_ALBA_ERROR, EXIT_INTERRUPTED};
use crate::render::{GroupedRenderer, InterleavedRenderer, JsonRenderer, LineSink, Renderer};

/// Runs `target` and returns the process exit code.
///
/// Loading already happened in `main.rs` (see the `commands` module doc
/// comment); `sources` is carried in only to render an [`EngineError`]
/// that points at a span in a Beamfile — an unknown target, or a `run`
/// template that fails at schedule time.
pub fn run(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
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
            LineSink::stderr().line(&format!("cannot start the async runtime: {error}"));
            return EXIT_ALBA_ERROR;
        }
    };

    runtime.block_on(execute(project, sources, beamfile, target, params, flags))
}

async fn execute(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    target: &BeamId,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
        cache: Some(CacheOptions {
            dir: cache_dir(beamfile),
            force: flags.force,
        }),
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
        cancel.clone(),
    )
    .await;

    // Every remaining event is already queued (the channel is unbounded),
    // and the engine dropped the last sender by returning — so this joins
    // once the renderer has drained the run, and cannot hang.
    //
    // A `JoinError` here means the renderer panicked, which is a bug in
    // Alba: whatever the run itself did, its report is now incomplete and
    // must not be passed off as a clean result. Reported and turned into
    // an Alba error rather than swallowed — swallowing it is precisely how
    // a broken renderer used to hand back exit 0 for a failing run.
    let mut err = LineSink::stderr();
    let renderer_panicked = consumer.await.is_err();
    if renderer_panicked {
        err.line("the output renderer panicked; this run's report is incomplete");
    }

    let code = match result {
        Ok(summary) => run_exit_code(&summary, &cancel),
        Err(error) => {
            err.line(render_engine_error(error, sources).trim_end());
            EXIT_ALBA_ERROR
        }
    };

    if renderer_panicked {
        EXIT_ALBA_ERROR
    } else {
        code
    }
}

/// The exit code a completed run reports.
///
/// [`RunSummary::exit_code`] answers "what did the beams earn" — `0` or
/// `1` — and a cancelled beam counts as `0` there, correctly: the engine
/// cannot know *why* it was cancelled. The CLI does. When Alba itself
/// cancelled the run because the user pressed Ctrl-C, the run did not
/// succeed, it was abandoned, and reporting `0` would let
/// `alba run deploy && ship` go on to ship after the user interrupted the
/// deploy. So an interrupted run reports [`EXIT_INTERRUPTED`], the same
/// `130` the double-interrupt path uses.
///
/// The token can only have been cancelled by [`watch_interrupts`]: the
/// engine's own fail-fast uses a child token, which does not propagate
/// upward.
fn run_exit_code(summary: &RunSummary, cancel: &CancellationToken) -> i32 {
    if cancel.is_cancelled() {
        EXIT_INTERRUPTED
    } else {
        summary.exit_code()
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
    let mut err = LineSink::stderr();
    let mut announced = false;
    loop {
        tokio::select! {
            event = incoming.recv() => match event {
                Some(event) => renderer.handle(&event),
                None => break,
            },
            () = cancel.cancelled(), if !announced => {
                announced = true;
                err.line("cancelling...");
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
/// commands and lets the run finish reporting normally — with
/// [`EXIT_INTERRUPTED`] rather than the code its beams earned, see
/// [`run_exit_code`]. Because stopping is bounded but not instant, a
/// second signal gives up on the orderly path and leaves immediately with
/// the same code. Commands still alive at that point are left running,
/// which is the user's explicit second-Ctrl-C request to stop waiting.
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
    LineSink::stderr().line("aborting");
    std::process::exit(EXIT_INTERRUPTED);
}

/// The cache directory for the project `beamfile` defines: `.alba/cache`
/// next to the Beamfile. A bare `Beamfile` path has an empty parent,
/// which means the current directory.
pub(crate) fn cache_dir(beamfile: &Path) -> std::path::PathBuf {
    beamfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(".alba")
        .join("cache")
}

/// How many beams may run at once, for this machine.
fn jobs(requested: Option<u64>) -> usize {
    resolve_jobs(requested, std::thread::available_parallelism().ok())
}

/// [`jobs`] with the platform's answer passed in, so the fallback can be
/// tested rather than only reasoned about — `available_parallelism` cannot
/// be made to fail on demand.
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
fn resolve_jobs(requested: Option<u64>, available: Option<NonZeroUsize>) -> usize {
    match requested {
        Some(requested) => usize::try_from(requested).unwrap_or(usize::MAX),
        None => available.map_or(1, NonZeroUsize::get),
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
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::BeamId;

    fn eight() -> Option<NonZeroUsize> {
        NonZeroUsize::new(8)
    }

    #[test]
    fn an_explicit_jobs_value_wins_over_the_platform_default() {
        assert_eq!(resolve_jobs(Some(3), eight()), 3);
    }

    #[test]
    fn an_omitted_jobs_value_follows_the_platform() {
        assert_eq!(resolve_jobs(None, eight()), 8);
    }

    /// The fallback that matters: a platform that cannot report its
    /// parallelism yields a sequential run, never zero slots.
    #[test]
    fn an_unknown_platform_parallelism_falls_back_to_one() {
        assert_eq!(resolve_jobs(None, None), 1);
    }

    /// A run nobody interrupted reports what its beams earned.
    #[test]
    fn an_uninterrupted_run_reports_the_summarys_code() {
        let failed = RunSummary {
            failed: vec![BeamId("bad".to_string())],
            ..RunSummary::default()
        };

        let cancel = CancellationToken::new();

        assert_eq!(run_exit_code(&RunSummary::default(), &cancel), 0);
        assert_eq!(run_exit_code(&failed, &cancel), 1);
    }

    /// A run Alba cancelled itself was abandoned, not completed — even
    /// though every one of its beams landed in the `cancelled` bucket,
    /// which `RunSummary::exit_code()` scores as 0.
    #[test]
    fn an_interrupted_run_reports_130_even_though_nothing_failed() {
        let cancelled = RunSummary {
            cancelled: vec![BeamId("slow".to_string())],
            ..RunSummary::default()
        };
        assert_eq!(cancelled.exit_code(), 0, "precondition");

        let cancel = CancellationToken::new();
        cancel.cancel();

        assert_eq!(run_exit_code(&cancelled, &cancel), EXIT_INTERRUPTED);
    }
}
