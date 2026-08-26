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
//!
//! ## `--watch`
//!
//! [`watch_execute`] keeps that shape and swaps the middle piece: the
//! engine's [`alba_engine::watch`] session loop replaces the single
//! [`alba_engine::run`], so one renderer and one interrupt watcher span
//! every run of the session rather than one run. The differences it does
//! carry are the file watcher it must build up front, the errors the
//! session hands back mid-flight for rendering, and its own exit codes.
//!
//! ## The interactive interface
//!
//! [`ui_enabled`] decides, before anything is built, which of the two front
//! ends this run gets. [`tui_execute`] is the interactive one: the same
//! session loop as [`watch_execute`], but its event stream feeds
//! [`alba_tui`] instead of a [`Renderer`], and the interface answers back
//! with [`alba_engine::SessionCommand`]s over a second channel. What the
//! session was told to do is therefore no longer fixed at startup — `--watch`
//! only sets where it *begins*. When the interface closes, [`replay`] puts
//! the last run's summary and the failed beams' logs back on stderr, which
//! the alternate screen would otherwise have taken with it.

use std::io::IsTerminal;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alba_core::{BeamId, Project, SourceMap};
use alba_engine::{
    CacheOptions, EngineError, Executors, RunEvent, RunOptions, RunSummary, Selection,
    SessionError, WatchExit,
};
use alba_executors::{DockerExecutor, EmbeddedShellExecutor, SystemShellExecutor};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::args::{LogFormat, OutputStyle, RunFlags};
use crate::exit::{EXIT_ALBA_ERROR, EXIT_INTERRUPTED};
use crate::render::{GroupedRenderer, InterleavedRenderer, JsonRenderer, LineSink, Renderer};

/// Runs `selection` and returns the process exit code.
///
/// Loading already happened in `main.rs` (see the `commands` module doc
/// comment); `sources` is carried in only to render an [`EngineError`]
/// that points at a span in a Beamfile — an unknown target, or a `run`
/// template that fails at schedule time.
pub fn run(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    selection: &Selection,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    // Answered before anything is built: a refusal has nothing to run, and
    // resolving it here leaves only the two front ends below.
    let interactive = match ui_enabled(flags, std::io::stdout().is_terminal()) {
        UiDecision::Tui => true,
        UiDecision::Headless => false,
        UiDecision::RefusedNoTty => {
            LineSink::stderr().line("--ui needs a terminal; stdout is not one");
            return EXIT_ALBA_ERROR;
        }
    };

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

    // The interface subsumes `--watch` — it can turn watching on and off
    // mid-session — so the flag only splits the two headless paths, which
    // are otherwise exactly the program they were.
    if interactive {
        runtime.block_on(tui_execute(
            project, sources, beamfile, selection, params, flags,
        ))
    } else if flags.watch {
        runtime.block_on(watch_execute(
            project, sources, beamfile, selection, params, flags,
        ))
    } else {
        runtime.block_on(execute(
            project, sources, beamfile, selection, params, flags,
        ))
    }
}

/// Every run's executor set. Docker mounts the project at the root
/// Beamfile's directory, resolved through the same helper the watcher
/// uses so the two never disagree on where the project is rooted.
fn executors(beamfile: &Path) -> Executors {
    Executors {
        embedded: Arc::new(EmbeddedShellExecutor),
        system: Arc::new(SystemShellExecutor),
        docker: Arc::new(DockerExecutor::new(alba_engine::beamfile_dir(beamfile))),
    }
}

/// Which of the two front ends a run gets.
#[derive(Debug, PartialEq, Eq)]
enum UiDecision {
    /// The interactive interface.
    Tui,
    /// The existing renderers, on stdout and stderr.
    Headless,
    /// `--ui` on something that is not a terminal: an error, not a fallback.
    RefusedNoTty,
}

/// The spec's activation table: the interface is the default on a terminal,
/// and yields to headless for a machine-readable stream (`--log-format
/// json`), an explicit text layout (`--output`, since asking for a layout
/// is asking for text), or `--no-ui`.
///
/// `--ui` forces it the other way, but it still needs a real terminal to
/// draw on. Off one it is *refused* rather than quietly downgraded: an
/// interface drawn down a pipe would fill the reader's stream with escape
/// codes, and silently ignoring the flag would hide the fact that what was
/// asked for is impossible here.
///
/// Takes the TTY answer as an argument rather than asking `IsTerminal`
/// itself, so the whole table can be tested — under a test harness stdout
/// is never a terminal, which would make half of it unreachable.
fn ui_enabled(flags: &RunFlags, stdout_is_tty: bool) -> UiDecision {
    if flags.no_ui || flags.log_format == LogFormat::Json || flags.output.is_some() {
        return UiDecision::Headless;
    }
    match (flags.ui, stdout_is_tty) {
        (_, true) => UiDecision::Tui,
        (true, false) => UiDecision::RefusedNoTty,
        (false, false) => UiDecision::Headless,
    }
}

async fn execute(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    selection: &Selection,
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
        extra_env: Vec::new(),
    };

    let root = alba_engine::beamfile_dir(beamfile);
    let targets = match alba_engine::select(project, sources, &root, selection) {
        Ok(targets) => targets,
        Err(error) => {
            LineSink::stderr().line(render_engine_error(&error, sources).trim_end());
            return EXIT_ALBA_ERROR;
        }
    };

    let (events, incoming) = unbounded_channel();
    let cancel = CancellationToken::new();

    tokio::spawn(watch_interrupts(cancel.clone()));
    let consumer = tokio::spawn(consume(incoming, renderer(flags), cancel.clone()));

    let result = alba_engine::run(
        project,
        &targets,
        options,
        executors(beamfile),
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
            err.line(render_engine_error(&error, sources).trim_end());
            EXIT_ALBA_ERROR
        }
    };

    if renderer_panicked {
        EXIT_ALBA_ERROR
    } else {
        code
    }
}

/// The `--watch` counterpart of [`execute`]: same renderer, same interrupt
/// watcher, but the engine's session loop instead of a single run.
///
/// The exit codes differ from a single run's on purpose. Every run in the
/// session already reported itself as it happened, so what a beam earned is
/// old news by the time the user ends the session: an orderly Ctrl-C is
/// `0`. [`EXIT_ALBA_ERROR`] is left for the failures that stop a session
/// from being a session at all — one that cannot start, and a watcher that
/// dies under it.
async fn watch_execute(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    selection: &Selection,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    let mut err = LineSink::stderr();

    let root = alba_engine::beamfile_dir(beamfile);
    if let Err(error) = alba_engine::select(project, sources, &root, selection) {
        err.line(render_engine_error(&error, sources).trim_end());
        return EXIT_ALBA_ERROR;
    }
    if let Some(target) = selection.watched_target()
        && let Ok(subgraph) = alba_core::execution_subgraph(project, target)
        && let Some(warning) = no_inputs_warning(project, target, &subgraph)
    {
        err.line(&warning);
    }

    let watcher = match alba_engine::NotifyWatcher::new(&watch_roots(beamfile, sources)) {
        Ok(watcher) => watcher,
        Err(error) => {
            err.line(&format!("cannot start the file watcher: {error}"));
            return EXIT_ALBA_ERROR;
        }
    };

    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
        cache: Some(CacheOptions {
            dir: cache_dir(beamfile),
            force: flags.force,
        }),
        extra_env: Vec::new(),
    };

    let (events, incoming) = unbounded_channel();
    let cancel = CancellationToken::new();

    tokio::spawn(watch_interrupts(cancel.clone()));
    let consumer = tokio::spawn(consume(incoming, watch_renderer(flags), cancel.clone()));

    let exit = alba_engine::watch(
        beamfile,
        project.clone(),
        sources.clone(),
        selection.clone(),
        options,
        executors(beamfile),
        events,
        cancel,
        Box::new(watcher),
        // Mid-session trouble is reported and survived, so this renders
        // and returns the diagnostic; only the session's own end decides
        // the exit code. The session turns the answer into a
        // `RunEvent::ProjectBroken` and sends it down the same channel as
        // every other event, so the renderer — not this closure — decides
        // where it goes.
        //
        // Both arms render against the sources the error itself carries,
        // never the map captured above: a session outlives the project it
        // started on, and a reload renumbers spans and source ids out from
        // under the startup map.
        &mut |error| match error {
            SessionError::Load(load) => crate::render_load_error(load),
            SessionError::Run { error, sources } => render_engine_error(error, sources),
        },
    )
    .await;

    // Same reasoning as [`execute`]: the engine dropped the last sender by
    // returning, so this joins on a drained renderer, and a `JoinError`
    // means it panicked and the session's report is incomplete.
    if consumer.await.is_err() {
        err.line("the output renderer panicked; this session's report is incomplete");
        return EXIT_ALBA_ERROR;
    }

    match exit {
        WatchExit::Interrupted => 0,
        WatchExit::WatcherClosed => {
            err.line("the file watcher stopped; ending the session");
            EXIT_ALBA_ERROR
        }
    }
}

/// The interactive counterpart of [`watch_execute`]: the same session loop,
/// driven by [`alba_tui`] instead of a renderer.
///
/// The startup rules are [`watch_execute`]'s, on purpose. A target this
/// project does not declare is still exit [`EXIT_ALBA_ERROR`] before
/// anything opens — mid-session the session parks on an unknown target,
/// but at startup it means the command line was wrong — and the watcher is
/// built whatever `--watch` says, because `w` can turn watching on at any
/// point in the session and a watcher cannot be added to a running one.
///
/// The one piece of [`execute`]'s shape that is deliberately absent is
/// [`watch_interrupts`]: raw mode delivers Ctrl-C to the interface as an
/// ordinary key event, which it answers itself (cancel a run, or quit),
/// and a signal handler racing it would cancel runs the user never asked
/// to cancel.
async fn tui_execute(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    selection: &Selection,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    let mut err = LineSink::stderr();

    // The exit-2-at-startup rule. The subgraph itself is not wanted here:
    // `no_inputs_warning`'s advice belongs to a session that can only
    // watch, and this one shows its watch state in the header and lets `w`
    // change it.
    let root = alba_engine::beamfile_dir(beamfile);
    if let Err(error) = alba_engine::select(project, sources, &root, selection) {
        err.line(render_engine_error(&error, sources).trim_end());
        return EXIT_ALBA_ERROR;
    }

    let watcher = match alba_engine::NotifyWatcher::new(&watch_roots(beamfile, sources)) {
        Ok(watcher) => watcher,
        Err(error) => {
            err.line(&format!("cannot start the file watcher: {error}"));
            return EXIT_ALBA_ERROR;
        }
    };

    let colour = crate::color_enabled();
    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
        cache: Some(CacheOptions {
            dir: cache_dir(beamfile),
            force: flags.force,
        }),
        // The interface renders ANSI, and commands on a pipe emit none
        // unless told to; a monochrome interface asks for none.
        extra_env: if colour {
            vec![
                ("FORCE_COLOR".to_string(), "1".to_string()),
                ("CLICOLOR_FORCE".to_string(), "1".to_string()),
            ]
        } else {
            Vec::new()
        },
    };

    let (events, incoming) = unbounded_channel();
    let (commands, command_receiver) = unbounded_channel();
    let cancel = CancellationToken::new();

    let session = tokio::spawn({
        // The session outlives this frame's borrows, so everything it
        // needs is cloned in — all of it cheap next to a single run.
        let beamfile = beamfile.to_path_buf();
        let project = project.clone();
        let sources = sources.clone();
        let selection = selection.clone();
        let watch = flags.watch;
        async move {
            alba_engine::session(
                &beamfile,
                project,
                sources,
                selection,
                options,
                executors(&beamfile),
                events,
                Some(command_receiver),
                watch,
                cancel,
                Box::new(watcher),
                // Same contract as [`watch_execute`]'s: render against the
                // sources the error carries, never a startup copy, and let
                // the session turn the answer into an event.
                &mut |error| match error {
                    SessionError::Load(load) => crate::render_load_error(load),
                    SessionError::Run { error, sources } => render_engine_error(error, sources),
                },
            )
            .await
        }
    });

    let outcome = alba_tui::run(
        incoming,
        commands,
        alba_tui::TuiOptions {
            target: selection_label(selection),
            watch: flags.watch,
            colour,
        },
    )
    .await;

    // The interface owns the last word either way: on its own exit it sent
    // `Shutdown`, and on a failure it dropped the command sender, which the
    // session reads as the same thing. So this joins on a session that is
    // already stopping, and cannot hang.
    let failure = session_failure(session.await);

    // Everything below writes to a terminal the guard restored when
    // `alba_tui::run` returned, so the alternate screen is gone and stderr
    // reaches the shell the user is left looking at.
    if let Some(message) = failure {
        err.line(message);
    }

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            err.line(&format!("the interface failed: {error}"));
            return EXIT_ALBA_ERROR;
        }
    };

    // The session's own fixed reference — `TuiOutcome` carries none of its
    // own, and a mid-session retarget to a plain beam (`SessionCommand::Run`)
    // is not reflected here, same as `selection_label` above: both read the
    // selection this call started with, not whatever the session moved to.
    let affected_by = match selection {
        Selection::Affected { reference, .. } => Some(reference.as_str()),
        Selection::Beam(_) => None,
    };
    replay(&mut err, &outcome, affected_by, colour);

    match failure {
        // A session that did not end on its own terms is an Alba failure,
        // the same [`EXIT_ALBA_ERROR`] the headless session reports for it.
        // The last run's code would claim a session that concluded, and
        // this one was cut off under the interface.
        Some(_) => EXIT_ALBA_ERROR,
        // No run that ran to completion means nothing here vouches for the
        // sources: an abandoned run, a parked session, or a session the
        // user quit before anything finished all report an interruption.
        None => outcome.last_run_code.unwrap_or(EXIT_INTERRUPTED),
    }
}

/// What the interface calls the session before its first run reports.
fn selection_label(selection: &Selection) -> String {
    match selection {
        Selection::Beam(id)
        | Selection::Affected {
            within: Some(id), ..
        } => id.0.clone(),
        Selection::Affected {
            reference,
            within: None,
        } => format!("affected by {reference}"),
    }
}

/// The stderr line a session owes when it did not end on its own terms,
/// `None` for the ordinary goodbye.
///
/// [`WatchExit::WatcherClosed`] is the one the interface *cannot* report
/// itself: the session ending closes the event channel, which the
/// interface reads as "nothing left to drive" and leaves on, silently — so
/// without this a watcher that died would take the whole session down
/// without a word, and with whatever the last run happened to earn. The
/// wording is the headless session's, deliberately: it is the same event.
///
/// A `JoinError` is a panic in the session task, which is a bug in Alba —
/// reported rather than swallowed, for the same reason [`execute`] reports
/// a panicking renderer: the report the interface just gave is incomplete
/// and must not pass for a clean result.
fn session_failure(exit: Result<WatchExit, tokio::task::JoinError>) -> Option<&'static str> {
    match exit {
        Ok(WatchExit::Interrupted) => None,
        Ok(WatchExit::WatcherClosed) => Some("the file watcher stopped; ending the session"),
        Err(_) => Some("the session ended unexpectedly; this session's report is incomplete"),
    }
}

/// What quitting the interface leaves behind on stderr: the last run's
/// summary, then each failed beam's output under its own header.
///
/// The alternate screen takes the whole session with it when it closes, so
/// without this a failing run would end on an empty prompt. The summary is
/// [`crate::render::print_summary`]'s, the very line the text renderers
/// print, so the two front ends cannot describe the same run differently.
///
/// The logs come from the interface's per-beam ring buffers, which are
/// capped: a beam that produced more than [`alba_tui::logs::MAX_LINES`]
/// lines is replayed from wherever the buffer starts, and says so rather
/// than passing a partial log off as the whole one. `colour` picks which
/// side of each buffered [`alba_tui::ReplayLine`] gets printed: `raw` on a
/// terminal, `text` (already stripped) under `NO_COLOR`.
///
/// Generic over the sink's writer for the same reason
/// [`crate::render::print_summary`] is: the caller hands it the real
/// stderr, while a test hands it a buffer and reads back exactly what
/// the user would have been left looking at.
///
/// `affected_by` is the caller's own selection, not anything read back off
/// `outcome` — `TuiOutcome` carries no reference of its own — so an
/// `--affected` session that ends on an empty run still owes the same
/// `nothing affected` line the headless and watch sessions print.
fn replay<W: std::io::Write>(
    err: &mut LineSink<W>,
    outcome: &alba_tui::TuiOutcome,
    affected_by: Option<&str>,
    colour: bool,
) {
    if let Some(summary) = &outcome.last_summary {
        crate::render::print_summary(err, summary, affected_by);
    }

    for (beam, lines) in &outcome.failed_logs {
        err.line(&format!("\u{2500}\u{2500} {beam} \u{2500}\u{2500}"));
        if lines.len() >= alba_tui::logs::MAX_LINES {
            err.line(&format!(
                "(the interface keeps at most {} lines per beam; anything earlier is not replayed)",
                alba_tui::logs::MAX_LINES
            ));
        }
        for line in lines {
            err.line(if colour { &line.raw } else { &line.text });
        }
    }
}

/// The warning a session that can only ever react to Beamfile edits earns,
/// or `None` when some beam in `subgraph` declares `inputs`.
///
/// Such a session is legitimate — a Beamfile edit reloads the project, so
/// adding the missing `inputs` repairs it in place — but a watch that
/// ignores every source file is surprising enough to say out loud, once,
/// before the session starts.
fn no_inputs_warning(project: &Project, target: &BeamId, subgraph: &[BeamId]) -> Option<String> {
    let declares_inputs = project
        .beams
        .iter()
        .filter(|beam| subgraph.contains(&beam.id))
        .any(|beam| !beam.inputs.is_empty());

    (!declares_inputs).then(|| {
        format!(
            "warning: no beam in `{}`'s graph declares inputs; watching the Beamfile only",
            target.0
        )
    })
}

/// The directories [`alba_engine::NotifyWatcher`] puts under recursive
/// watch: the project root, plus the directory of any Beamfile loaded from
/// outside it (an import in a sibling tree).
///
/// Roots are fixed for the session. An import *added mid-session* that
/// lives outside these roots emits no events until the next
/// `alba run --watch` — a known limitation, not an oversight: re-deriving
/// the roots would mean tearing down and rebuilding the watcher on every
/// Beamfile save.
fn watch_roots(beamfile: &Path, sources: &SourceMap) -> Vec<PathBuf> {
    roots_of(beamfile, sources.paths())
}

/// [`watch_roots`] over the loaded paths themselves, so the rules can be
/// tested — a [`SourceMap`] can only be produced by loading a real project.
/// Both the roots and the paths a session displays are rooted by
/// [`alba_engine::beamfile_dir`], never by a rule spelled again here:
/// keeping two notions of "the directory this Beamfile is rooted at" is
/// exactly how a watcher that started correctly ends up reporting paths
/// the status line cannot shorten.
fn roots_of<'a>(beamfile: &Path, loaded: impl Iterator<Item = &'a Path>) -> Vec<PathBuf> {
    let root = alba_engine::beamfile_dir(beamfile);
    let mut roots = vec![root.clone()];
    for path in loaded {
        let dir = alba_engine::beamfile_dir(path);
        if !dir.starts_with(&root) && !roots.contains(&dir) {
            roots.push(dir);
        }
    }
    roots
}

/// The watch session's renderer: the same selection [`renderer`] makes, but
/// the text renderers clear the screen between runs — a clean page per run,
/// the way a watch mode is expected to read.
///
/// Only when stdout is a terminal, and never for JSON: a machine parsing
/// the stream has no screen to clear, and off a terminal the output is a
/// log accumulating in a file, where erasing what came before destroys the
/// record.
fn watch_renderer(flags: &RunFlags) -> Box<dyn Renderer> {
    let clear = std::io::stdout().is_terminal();
    match flags.log_format {
        LogFormat::Json => Box::new(JsonRenderer::new()),
        LogFormat::Text => match flags.output.unwrap_or_else(default_output) {
            OutputStyle::Interleaved => {
                Box::new(InterleavedRenderer::new(crate::color_enabled(), clear))
            }
            OutputStyle::Grouped => Box::new(GroupedRenderer::new(clear)),
        },
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
            OutputStyle::Interleaved => {
                Box::new(InterleavedRenderer::new(crate::color_enabled(), false))
            }
            OutputStyle::Grouped => Box::new(GroupedRenderer::new(false)),
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
///
/// By reference for the same reason [`crate::render_load_error`] is: a
/// watch session survives the failures it reports, so it only ever lends
/// them out for rendering.
fn render_engine_error(error: &EngineError, sources: &SourceMap) -> String {
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

    /// A beam declaring `inputs`, with everything else left inert — these
    /// tests only ever ask which files a beam watches.
    fn beam(id: &str, inputs: &[&str]) -> alba_core::Beam {
        alba_core::Beam {
            id: BeamId(id.to_string()),
            description: None,
            needs: Vec::new(),
            params: Vec::new(),
            inputs: inputs.iter().map(|input| (*input).to_string()).collect(),
            outputs: Vec::new(),
            run: Vec::new(),
            env: Vec::new(),
            cwd: None,
            executor: alba_core::ExecutorKind::Shell,
            allow_failure: false,
            dir: PathBuf::from("."),
            span: alba_syntax::Span::new(0, 0),
            source: alba_core::SourceId(0),
            scope: alba_core::Scope::empty(),
        }
    }

    fn project(beams: Vec<alba_core::Beam>) -> Project {
        Project {
            beams,
            default: None,
            hooks: Vec::new(),
        }
    }

    fn ids(ids: &[&str]) -> Vec<BeamId> {
        ids.iter().map(|id| BeamId((*id).to_string())).collect()
    }

    /// The warning's whole point: a subgraph with nothing to watch would
    /// sit there reacting to Beamfile saves alone.
    #[test]
    fn a_subgraph_without_inputs_is_warned_about() {
        let project = project(vec![beam("build", &[])]);

        let warning = no_inputs_warning(&project, &BeamId("build".to_string()), &ids(&["build"]));

        let warning = warning.expect("a graph with no inputs must be warned about");
        assert!(warning.contains("build"), "got: {warning}");
    }

    /// One beam anywhere in the subgraph is enough: the target itself
    /// declares nothing, but running it means running its dependency, and
    /// that one's inputs will trigger the session.
    #[test]
    fn inputs_on_a_dependency_are_enough_to_stay_silent() {
        let project = project(vec![beam("build", &[]), beam("compile", &["src/**"])]);

        assert_eq!(
            no_inputs_warning(
                &project,
                &BeamId("build".to_string()),
                &ids(&["build", "compile"])
            ),
            None
        );
    }

    /// Only the target's own subgraph counts. A beam this run will never
    /// execute cannot trigger anything, so its inputs must not buy silence.
    #[test]
    fn inputs_outside_the_subgraph_do_not_count() {
        let project = project(vec![beam("build", &[]), beam("docs", &["docs/**"])]);

        assert!(
            no_inputs_warning(&project, &BeamId("build".to_string()), &ids(&["build"])).is_some()
        );
    }

    /// The regression that matters: `alba run --watch` without `--file`
    /// resolves to a bare `Beamfile`, whose parent is the empty path — and
    /// `notify` rejects an empty path outright, so a session that let one
    /// through could not start at all.
    #[test]
    fn a_bare_beamfile_watches_the_current_directory() {
        let bare = Path::new("Beamfile");

        let roots = roots_of(bare, [bare].into_iter());

        let here = std::path::absolute(".").unwrap();
        assert_eq!(roots, vec![here]);
    }

    /// A Beamfile inside the project root — the root file itself, and any
    /// import below it — is already covered by the root's recursive watch.
    #[test]
    fn beamfiles_under_the_root_add_no_roots() {
        let root = std::path::absolute("project").unwrap();
        let beamfile = root.join("Beamfile");
        let nested = root.join("modules/api/Beamfile");

        let roots = roots_of(
            &beamfile,
            [beamfile.as_path(), nested.as_path()].into_iter(),
        );

        assert_eq!(roots, vec![root]);
    }

    /// An import from a sibling tree is outside the root's recursive watch,
    /// so its own directory joins the roots — once, however many of its
    /// files were loaded.
    #[test]
    fn a_beamfile_outside_the_root_adds_its_directory_once() {
        let root = std::path::absolute("project").unwrap();
        let beamfile = root.join("Beamfile");
        let sibling = std::path::absolute("shared").unwrap();
        let (first, second) = (sibling.join("A.beam"), sibling.join("B.beam"));

        let roots = roots_of(
            &beamfile,
            [beamfile.as_path(), first.as_path(), second.as_path()].into_iter(),
        );

        assert_eq!(roots, vec![root, sibling]);
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

    /// The default: a terminal gets the interface, anything else — a pipe,
    /// a CI log — stays on the headless renderers.
    #[test]
    fn the_tui_is_the_default_on_a_terminal_only() {
        let flags = RunFlags::default();

        assert_eq!(ui_enabled(&flags, true), UiDecision::Tui);
        assert_eq!(ui_enabled(&flags, false), UiDecision::Headless);
    }

    /// Asking for a machine stream or for a named text layout is asking for
    /// text, and `--no-ui` says it outright — all three win over the
    /// terminal the user happens to be sitting at.
    #[test]
    fn asking_for_a_text_layout_or_json_means_headless() {
        let json = RunFlags {
            log_format: LogFormat::Json,
            ..RunFlags::default()
        };
        let text = RunFlags {
            output: Some(OutputStyle::Grouped),
            ..RunFlags::default()
        };
        let off = RunFlags {
            no_ui: true,
            ..RunFlags::default()
        };

        for flags in [json, text, off] {
            assert_eq!(ui_enabled(&flags, true), UiDecision::Headless);
        }
    }

    /// `--ui` forces the interface, but it cannot draw one down a pipe:
    /// refused outright rather than filling the reader's stream with
    /// escape codes.
    #[test]
    fn forcing_the_ui_off_a_terminal_is_refused_not_garbled() {
        let flags = RunFlags {
            ui: true,
            ..RunFlags::default()
        };

        assert_eq!(ui_enabled(&flags, false), UiDecision::RefusedNoTty);
        assert_eq!(ui_enabled(&flags, true), UiDecision::Tui);
    }

    /// `--no-ui` wins even when `--ui` is somehow also set: the flags
    /// conflict at parse time, so this only pins the rule's own order.
    #[test]
    fn turning_the_ui_off_wins_over_forcing_it_on() {
        let flags = RunFlags {
            ui: true,
            no_ui: true,
            ..RunFlags::default()
        };

        assert_eq!(ui_enabled(&flags, true), UiDecision::Headless);
        assert_eq!(ui_enabled(&flags, false), UiDecision::Headless);
    }

    /// What `replay` wrote, as lines under `NO_COLOR`: the interface is
    /// gone by the time it runs, so this is literally what the user is
    /// left looking at.
    fn replayed(outcome: &alba_tui::TuiOutcome, affected_by: Option<&str>) -> Vec<String> {
        replayed_with(outcome, affected_by, false)
    }

    /// Same as [`replayed`], but choosing whether the replay keeps the
    /// compiler's own colour (`colour: true`, `line.raw`) or strips it
    /// (`colour: false`, `line.text`).
    fn replayed_with(
        outcome: &alba_tui::TuiOutcome,
        affected_by: Option<&str>,
        colour: bool,
    ) -> Vec<String> {
        let mut buffer: Vec<u8> = Vec::new();
        {
            let mut sink = LineSink::new(&mut buffer);
            replay(&mut sink, outcome, affected_by, colour);
        }
        String::from_utf8(buffer)
            .expect("the replay writes UTF-8")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Wraps plain strings into escape-free `ReplayLine`s (`raw` and `text`
    /// the same), for tests that do not care about the colour distinction.
    fn plain_lines(lines: Vec<String>) -> Vec<alba_tui::ReplayLine> {
        lines
            .into_iter()
            .map(|line| alba_tui::ReplayLine {
                raw: line.clone(),
                text: line,
            })
            .collect()
    }

    fn outcome(
        summary: Option<RunSummary>,
        failed_logs: Vec<(String, Vec<alba_tui::ReplayLine>)>,
    ) -> alba_tui::TuiOutcome {
        alba_tui::TuiOutcome {
            last_run_code: summary.as_ref().map(RunSummary::exit_code),
            last_summary: summary,
            failed_logs,
        }
    }

    /// A green run leaves the summary and nothing else: no beam failed,
    /// so there is no output to replay under a header.
    #[test]
    fn the_replay_of_a_green_run_is_its_summary() {
        let summary = RunSummary {
            succeeded: vec![BeamId("ok".to_string())],
            ..RunSummary::default()
        };

        let lines = replayed(&outcome(Some(summary), Vec::new()), None);

        assert_eq!(lines.len(), 1, "got: {lines:?}");
        assert!(
            lines[0].contains("1 succeeded"),
            "the summary the text renderers print, got: {lines:?}"
        );
    }

    /// A failing run leaves the summary, then each failed beam's own
    /// buffered output under its own header — the whole point of the
    /// replay, since the alternate screen took the run with it.
    #[test]
    fn the_replay_of_a_failing_beam_carries_its_buffered_output() {
        let summary = RunSummary {
            failed: vec![BeamId("bad".to_string())],
            ..RunSummary::default()
        };
        let logs = vec![(
            "bad".to_string(),
            plain_lines(vec!["boom 1".to_string(), "boom 2".to_string()]),
        )];

        let lines = replayed(&outcome(Some(summary), logs), None);

        assert!(lines[0].contains("1 failed"), "got: {lines:?}");
        assert_eq!(
            &lines[1..],
            ["\u{2500}\u{2500} bad \u{2500}\u{2500}", "boom 1", "boom 2"],
            "the beam's header, then every line it printed"
        );
    }

    /// A beam that overflowed the interface's ring buffer is replayed
    /// from wherever the buffer starts, and says so rather than passing
    /// a partial log off as the whole one.
    #[test]
    fn a_truncated_beams_replay_discloses_what_is_missing() {
        let summary = RunSummary {
            failed: vec![BeamId("chatty".to_string())],
            ..RunSummary::default()
        };
        let logs = vec![(
            "chatty".to_string(),
            plain_lines(
                (0..alba_tui::logs::MAX_LINES)
                    .map(|index| format!("line {index}"))
                    .collect::<Vec<_>>(),
            ),
        )];

        let lines = replayed(&outcome(Some(summary), logs), None);

        assert_eq!(lines[1], "\u{2500}\u{2500} chatty \u{2500}\u{2500}");
        assert!(
            lines[2].contains(&format!("at most {} lines", alba_tui::logs::MAX_LINES)),
            "the disclosure comes before the lines themselves, got: {:?}",
            lines[2]
        );
        assert_eq!(lines[3], "line 0");
        assert_eq!(lines.len(), 3 + alba_tui::logs::MAX_LINES);
    }

    /// A session that quit before any run finished has nothing to say:
    /// no summary, no beams, and so not a single line.
    #[test]
    fn a_session_with_no_finished_run_replays_nothing() {
        assert!(replayed(&outcome(None, Vec::new()), None).is_empty());
    }

    /// The reviewer's finding: `TuiOutcome` carries no `affected_by` of its
    /// own, but the caller's selection does, and an `--affected` session
    /// that ends right after an empty run must still say so on the way
    /// out — not fall back to a bare, misleading duration.
    #[test]
    fn the_replay_of_an_empty_affected_run_says_nothing_was_affected() {
        let lines = replayed(
            &outcome(Some(RunSummary::default()), Vec::new()),
            Some("HEAD"),
        );

        assert_eq!(lines, ["\u{2713} nothing affected by HEAD"]);
    }

    /// The replay keeps the compiler's colours on a terminal and drops
    /// them under NO_COLOR.
    #[test]
    fn the_replay_prints_raw_lines_with_colour_and_plain_lines_without() {
        let summary = RunSummary {
            failed: vec![BeamId("red".to_string())],
            ..RunSummary::default()
        };
        let line = alba_tui::ReplayLine {
            raw: "\u{1b}[31mred\u{1b}[0m".to_string(),
            text: "red".to_string(),
        };
        let outcome = outcome(Some(summary), vec![("red".to_string(), vec![line])]);

        let coloured = replayed_with(&outcome, None, true);
        assert!(
            coloured.iter().any(|l| l == "\u{1b}[31mred\u{1b}[0m"),
            "{coloured:?}"
        );
        let plain = replayed_with(&outcome, None, false);
        assert!(plain.iter().any(|l| l == "red"), "{plain:?}");
        assert!(plain.iter().all(|l| !l.contains('\u{1b}')), "{plain:?}");
    }

    /// An orderly goodbye is not a failure and owes no line.
    #[test]
    fn an_interrupted_session_is_not_a_failure() {
        assert_eq!(session_failure(Ok(WatchExit::Interrupted)), None);
    }

    /// The one the interface cannot report itself: the session ending
    /// closes its event channel, which reads as "nothing left to drive",
    /// so the screen would just vanish without a word.
    #[test]
    fn a_dead_watcher_is_reported_the_way_the_headless_session_reports_it() {
        let message = session_failure(Ok(WatchExit::WatcherClosed))
            .expect("a session whose watcher died must say so");
        assert!(message.contains("file watcher"), "got: {message}");
    }

    /// A panic in the session task is Alba's own bug, and it means the
    /// report the interface just gave is incomplete — never a clean exit.
    #[tokio::test]
    async fn a_session_that_panicked_is_a_failure() {
        let panicked = tokio::spawn(async { panic!("the session fell over") })
            .await
            .expect_err("the task must have panicked");

        assert!(session_failure(Err(panicked)).is_some());
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
