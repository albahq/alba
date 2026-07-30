//! Watch mode: run the target, then re-run it whenever the files its
//! subgraph declares as `inputs` change.
//!
//! ## Shape
//!
//! [`watch`] owns the session: build the `WatchSet`, run the target
//! through the ordinary scheduler (the cache does the incremental work),
//! then wait for the [`Watcher`]'s debounced batches. A relevant batch
//! during the wait starts the next run; one during a run cancels that
//! run first — latest code wins — and the trigger event is emitted after
//! the cancelled run's summary, which is what marks it as interrupted by
//! the watch. The caller's token ends the session; each run gets a child
//! token so a restart never looks like a user interrupt.
//!
//! A trigger that names a loaded Beamfile reloads the project first, so
//! the next run schedules the beams as they are now written. A project
//! the session cannot work from — one that no longer parses, one whose
//! target beam is gone — parks the loop in
//! [`park_until_the_project_changes`]: reported, announced as a wait,
//! executing nothing, until a save makes it loadable again.
//!
//! ## Why the loop reports errors through a callback
//!
//! A mid-session failure (a run that cannot be scheduled, a broken
//! Beamfile) must be *rendered* — spans, carets, suggestions — and
//! rendering lives in the CLI, above this crate. Embedding these errors
//! in [`crate::RunEvent`] would force `Clone` and a wire format on types
//! that exist to be pretty-printed once; a callback keeps the event
//! channel's contract clean and the session alive after reporting.

mod notify;
mod set;

use std::path::{Path, PathBuf};

use alba_core::{BeamId, Project, SourceMap};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::EngineError;
use crate::event::RunEvent;
use crate::scheduler::{Executors, RunOptions, run};
pub use notify::NotifyWatcher;
use set::{Relevance, WatchSet};

/// One delivery from a file watcher.
pub enum WatchBatch {
    /// The paths the debouncer coalesced into this batch.
    Paths(Vec<PathBuf>),
    /// The watcher lost track (queue overflow): something changed, but
    /// it cannot say what. Treated as a trigger with no named paths —
    /// the cache absorbs the imprecision.
    Rescan,
}

/// A stream of debounced change batches. The indirection exists for the
/// engine's own tests, which script batches instead of touching a real
/// file system.
#[async_trait::async_trait]
pub trait Watcher: Send {
    /// The next batch; `None` when the watcher died for good.
    ///
    /// Must be cancel-safe: the session polls it in a `select!` against
    /// the running build and drops the future when the run finishes
    /// first, so a batch that was already taken from the underlying
    /// stream would be lost with it.
    async fn next_batch(&mut self) -> Option<WatchBatch>;
}

/// Why a session ended. Sessions have no failure exit — mid-session
/// trouble is reported and survived — so this is the complete list.
pub enum WatchExit {
    /// The caller's token fired: the user is done.
    Interrupted,
    /// The watcher's stream ended. The session cannot honestly continue:
    /// idling while watching nothing would look exactly like a healthy
    /// quiet session.
    WatcherClosed,
}

/// Mid-session trouble, handed to the caller's `on_error` for rendering.
///
/// Both variants carry the sources their spans index, because a session
/// outlives the project it started on: by the time an error is reported,
/// any number of reloads may have replaced the map the caller was holding
/// when it started the session. Rendering against that stale map would
/// draw the caret on text the error was never about.
pub enum SessionError {
    /// A Beamfile stopped loading. [`alba_core::LoadError`] already pairs
    /// the failure with every file read before it happened.
    Load(alba_core::LoadError),
    /// A run could not be carried out: an unknown target, or a beam the
    /// scheduler refuses. Reported rather than fatal — the fix is one
    /// Beamfile save away.
    Run {
        error: EngineError,
        /// The session's sources as of this failure. An
        /// [`EngineError::Core`] carries a span and a
        /// [`alba_core::SourceId`] that only this map resolves — the
        /// scheduler stamps each one with the id of the file the offending
        /// beam was declared in, precisely so the caret lands there.
        sources: SourceMap,
    },
}

/// Runs `target` and keeps re-running it as long as `watcher` reports
/// relevant changes, until `cancel` fires or the watcher dies.
///
/// `project` and `sources` are taken owned: the session outlives the
/// caller's own copy, and both are cheap to clone.
///
/// A `target` this project does not declare is *not* an error here: it
/// parks the session like any other project it cannot work from, because
/// mid-session the fix is one Beamfile save away and a session that quit
/// on a renamed beam would be a session that quits while the user is
/// typing. A caller that wants the other answer — the exit code 2 an
/// unknown target earns on the command line — asks
/// [`alba_core::execution_subgraph`] before starting the session, which is
/// exactly what the CLI does: it is only at startup that "no such beam"
/// means the invocation was wrong rather than the edit unfinished.
#[allow(clippy::too_many_arguments)]
pub async fn watch(
    beamfile: &Path,
    mut project: Project,
    mut sources: SourceMap,
    target: BeamId,
    options: RunOptions,
    executors: Executors,
    events: UnboundedSender<RunEvent>,
    cancel: CancellationToken,
    mut watcher: Box<dyn Watcher>,
    on_error: &mut (dyn FnMut(&SessionError) + Send),
) -> WatchExit {
    let root = beamfile.parent().map(Path::to_path_buf).unwrap_or_default();
    // `--force` empties the cache's read side for the initial run only:
    // applying it to every triggered run would re-run the whole subgraph
    // on every keystroke, which is exactly what the cache is here to
    // prevent.
    let mut force_next = options.cache.as_ref().is_some_and(|cache| cache.force);

    loop {
        // Rebuilt for each run: the set caches the files its patterns
        // resolved to, and the run just before may have created or
        // deleted some of them.
        let mut set = match WatchSet::new(&project, &target, &sources) {
            Ok(set) => set,
            Err(error) => {
                // The target is not in this project — a beam renamed by
                // the save that got us here, most often. Nothing but a
                // different Beamfile can change that answer, so park on
                // one instead of re-running the same failure.
                on_error(&SessionError::Run {
                    error: EngineError::Core(error),
                    sources: sources.clone(),
                });
                match park_until_the_project_changes(
                    beamfile,
                    sources.clone(),
                    &events,
                    &mut *watcher,
                    &cancel,
                    on_error,
                )
                .await
                {
                    Reloaded::Project(fresh_project, fresh_sources) => {
                        (project, sources) = (fresh_project, fresh_sources);
                        announce_recovery(&events);
                        continue;
                    }
                    Reloaded::Exit(exit) => return exit,
                }
            }
        };

        // ---- run phase -------------------------------------------------
        let run_cancel = cancel.child_token();
        let mut run_options = options.clone();
        if let Some(cache) = run_options.cache.as_mut() {
            cache.force = force_next;
        }
        force_next = false;

        // A batch that lands while the run is in flight cancels it and is
        // held here, so the wait phase below starts the next run at once
        // instead of announcing a wait nobody is waiting for.
        let mut pending: Option<Trigger> = None;
        let result = {
            let run_future = run(
                &project,
                &target,
                run_options,
                executors.clone(),
                events.clone(),
                run_cancel.clone(),
            );
            tokio::pin!(run_future);
            loop {
                tokio::select! {
                    result = &mut run_future => break result,
                    batch = watcher.next_batch() => match batch {
                        Some(batch) => {
                            if let Some(trigger) = relevant(&mut set, batch) {
                                merge(&mut pending, trigger);
                                run_cancel.cancel();
                            }
                        }
                        None => {
                            // Let the run wind down before leaving, so the
                            // session never abandons running commands.
                            run_cancel.cancel();
                            let _ = (&mut run_future).await;
                            return WatchExit::WatcherClosed;
                        }
                    },
                }
            }
        };
        if let Err(error) = result {
            on_error(&SessionError::Run {
                error,
                sources: sources.clone(),
            });
        }
        if cancel.is_cancelled() {
            return WatchExit::Interrupted;
        }

        // ---- wait phase ------------------------------------------------
        let trigger = match pending.take() {
            Some(trigger) => trigger,
            None => {
                let _ = events.send(RunEvent::WatchWaiting {
                    files: set.file_count(),
                });
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return WatchExit::Interrupted,
                        batch = watcher.next_batch() => match batch {
                            Some(batch) => {
                                if let Some(trigger) = relevant(&mut set, batch) {
                                    break trigger;
                                }
                            }
                            None => return WatchExit::WatcherClosed,
                        },
                    }
                }
            }
        };
        let _ = events.send(RunEvent::WatchTriggered {
            paths: display_paths(&root, &trigger.paths),
        });

        // ---- reload phase ----------------------------------------------
        // The next iteration schedules the beams as they are now written,
        // not as they were when the session started. Whichever way the
        // project comes back, it is the loop's next iteration that rebuilds
        // the watched set and runs.
        if trigger.beamfile {
            match alba_core::load_project(beamfile) {
                Ok((fresh_project, fresh_sources)) => {
                    (project, sources) = (fresh_project, fresh_sources);
                }
                Err(error) => {
                    let rejected = error.sources.clone();
                    on_error(&SessionError::Load(error));
                    match park_until_the_project_changes(
                        beamfile,
                        rejected,
                        &events,
                        &mut *watcher,
                        &cancel,
                        on_error,
                    )
                    .await
                    {
                        Reloaded::Project(fresh_project, fresh_sources) => {
                            (project, sources) = (fresh_project, fresh_sources);
                            announce_recovery(&events);
                        }
                        Reloaded::Exit(exit) => return exit,
                    }
                }
            }
        }
    }
}

/// A batch the session acts on.
struct Trigger {
    /// The changed paths that concern the session. Empty is a trigger
    /// too — that is what a rescan looks like, a change the watcher
    /// cannot name.
    paths: Vec<PathBuf>,
    /// Whether one of them is a loaded Beamfile, in which case the
    /// project is reloaded before the next run.
    beamfile: bool,
}

/// What a batch amounts to once classified, `None` when none of its paths
/// concern the session.
fn relevant(set: &mut WatchSet, batch: WatchBatch) -> Option<Trigger> {
    match batch {
        // The watcher cannot say what changed, so the Beamfile is among
        // the candidates; a reload is one file read and a parse, far
        // cheaper than running a stale project until the next save.
        WatchBatch::Rescan => Some(Trigger {
            paths: Vec::new(),
            beamfile: true,
        }),
        WatchBatch::Paths(paths) => {
            let mut trigger = Trigger {
                paths: Vec::new(),
                beamfile: false,
            };
            for path in paths {
                match set.classify(&path) {
                    Relevance::Beamfile => {
                        trigger.beamfile = true;
                        trigger.paths.push(path);
                    }
                    Relevance::Input => trigger.paths.push(path),
                    Relevance::Irrelevant => {}
                }
            }
            (!trigger.paths.is_empty()).then_some(trigger)
        }
    }
}

/// Folds a fresh trigger into the one already held, so a burst of batches
/// during a single run announces one run over their union.
fn merge(pending: &mut Option<Trigger>, fresh: Trigger) {
    match pending {
        Some(existing) => {
            existing.beamfile |= fresh.beamfile;
            for path in fresh.paths {
                if !existing.paths.contains(&path) {
                    existing.paths.push(path);
                }
            }
        }
        None => *pending = Some(fresh),
    }
}

/// Root-relative display strings with `/` separators, deduplicated,
/// sorted for stable output. A path outside the root (an import's input
/// in a sibling directory) displays as-is.
fn display_paths(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let canonical_root = root.canonicalize();
    let mut display: Vec<String> = paths
        .iter()
        .map(|path| {
            relative_to(root, canonical_root.as_deref().ok(), path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    display.sort();
    display.dedup();
    display
}

/// `path` seen from the project root, or `path` itself when it lies
/// outside it.
///
/// The paths are compared as they came before anything is resolved,
/// because `canonicalize` is `realpath(3)` and fails outright on a file
/// that no longer exists — and a deletion (an `rm`, a `git checkout`, the
/// first half of a rename) is an ordinary watch event, whose path
/// `WatchSet` still classifies as an input from its snapshot. Resolving
/// only settles a disagreement about symlinks between what the watcher
/// reports and how the root was spelled: the root's canonical form works
/// on a deleted file too, its own does not and is the last attempt.
fn relative_to(root: &Path, canonical_root: Option<&Path>, path: &Path) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(root) {
        return relative.to_path_buf();
    }
    if let Some(base) = canonical_root
        && let Ok(relative) = path.strip_prefix(base)
    {
        return relative.to_path_buf();
    }
    if let Some(base) = canonical_root
        && let Ok(canonical) = path.canonicalize()
        && let Ok(relative) = canonical.strip_prefix(base)
    {
        return relative.to_path_buf();
    }
    path.to_path_buf()
}

/// Announces the run that a recovered project is about to get.
///
/// The asymmetry with the reload that succeeds outright, which stays
/// silent: there, the trigger just emitted announced the Beamfile change,
/// and the run is that change being answered — one trigger, one cycle. A
/// recovery has no such trigger to lean on. The one it could point at
/// announced the *breaking* edit, and that cycle was closed by an error
/// report; without this, the fixing save would produce a full run of
/// output out of nowhere, leaving the user unable to tell whether what
/// they are reading is the answer to their fix.
///
/// No paths are named: the session parked on batches it deliberately
/// never classified, so it does not know which of them was the fix.
fn announce_recovery(events: &UnboundedSender<RunEvent>) {
    let _ = events.send(RunEvent::WatchTriggered { paths: Vec::new() });
}

/// How the broken-project idle state ended.
enum Reloaded {
    /// A Beamfile that loads again, and the sources it came from.
    Project(Project, SourceMap),
    /// The session ended before one arrived.
    Exit(WatchExit),
}

/// The broken-project idle state: the last load — or the watched set built
/// from it — failed and was reported; nothing may execute until a project
/// the session can work from exists again. `rejected` is the project text
/// that report was about.
///
/// Every subsequent batch retries the load rather than being classified
/// first: the stale `WatchSet` could still name the Beamfiles it knew
/// about, but not one that a fixed `import` line has only just added, and
/// a retry is one file read plus a parse. What is *not* repeated is the
/// answer: a load whose sources come back byte-for-byte what was already
/// reported on says nothing new, and reprinting it would bury the
/// diagnostic under one copy per write anywhere in the watched tree — an
/// editor's temporary files, an LSP, a build writing its own artifacts.
/// Loading is deterministic, so equal sources mean the same outcome and
/// the same rendering; anything that could change the answer, including
/// the appearance of a file no load ever managed to read, changes them.
async fn park_until_the_project_changes(
    beamfile: &Path,
    mut rejected: SourceMap,
    events: &UnboundedSender<RunEvent>,
    watcher: &mut dyn Watcher,
    cancel: &CancellationToken,
    on_error: &mut (dyn FnMut(&SessionError) + Send),
) -> Reloaded {
    // Without this the stream's last word is the trigger that means
    // "running", and a consumer would read a stopped session as a busy
    // one for as long as the project stays broken. Zero files is the
    // honest count: nothing is resolved while nothing loads.
    let _ = events.send(RunEvent::WatchWaiting { files: 0 });

    loop {
        tokio::select! {
            () = cancel.cancelled() => return Reloaded::Exit(WatchExit::Interrupted),
            batch = watcher.next_batch() => match batch {
                Some(_) => match alba_core::load_project(beamfile) {
                    Ok((project, sources)) => {
                        if sources != rejected {
                            return Reloaded::Project(project, sources);
                        }
                    }
                    Err(error) => {
                        if error.sources != rejected {
                            rejected = error.sources.clone();
                            on_error(&SessionError::Load(error));
                        }
                    }
                },
                None => return Reloaded::Exit(WatchExit::WatcherClosed),
            },
        }
    }
}
