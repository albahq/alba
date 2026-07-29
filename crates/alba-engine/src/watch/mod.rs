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
//! ## Why the loop reports errors through a callback
//!
//! A mid-session failure (a run that cannot be scheduled, a broken
//! Beamfile) must be *rendered* — spans, carets, suggestions — and
//! rendering lives in the CLI, above this crate. Embedding these errors
//! in [`crate::RunEvent`] would force `Clone` and a wire format on types
//! that exist to be pretty-printed once; a callback keeps the event
//! channel's contract clean and the session alive after reporting.

mod set;

use std::path::{Path, PathBuf};

use alba_core::{BeamId, Project, SourceMap};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::EngineError;
use crate::event::RunEvent;
use crate::scheduler::{Executors, RunOptions, run};
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
pub enum SessionError {
    /// A Beamfile stopped loading.
    Load(alba_core::LoadError),
    /// A run could not be carried out: an unknown target, or a beam the
    /// scheduler refuses. Reported rather than fatal — the fix is one
    /// Beamfile save away.
    Run(EngineError),
}

/// Runs `target` and keeps re-running it as long as `watcher` reports
/// relevant changes, until `cancel` fires or the watcher dies.
///
/// `project` and `sources` are taken owned: the session outlives the
/// caller's own copy, and both are cheap to clone.
#[allow(clippy::too_many_arguments)]
pub async fn watch(
    beamfile: &Path,
    project: Project,
    sources: SourceMap,
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
                on_error(&SessionError::Run(EngineError::Core(error)));
                return idle_until_the_session_ends(&mut *watcher, &cancel).await;
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
        let mut pending: Option<Vec<PathBuf>> = None;
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
                            if let Some(paths) = relevant(&mut set, batch) {
                                merge(&mut pending, paths);
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
            on_error(&SessionError::Run(error));
        }
        if cancel.is_cancelled() {
            return WatchExit::Interrupted;
        }

        // ---- wait phase ------------------------------------------------
        let paths = match pending.take() {
            Some(paths) => paths,
            None => {
                let _ = events.send(RunEvent::WatchWaiting {
                    files: set.file_count(),
                });
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return WatchExit::Interrupted,
                        batch = watcher.next_batch() => match batch {
                            Some(batch) => {
                                if let Some(paths) = relevant(&mut set, batch) {
                                    break paths;
                                }
                            }
                            None => return WatchExit::WatcherClosed,
                        },
                    }
                }
            }
        };
        let _ = events.send(RunEvent::WatchTriggered {
            paths: display_paths(&root, &paths),
        });
    }
}

/// What a batch amounts to once classified: the paths that concern the
/// session, `None` when none of them do. An empty vector is a trigger
/// too — that is what a rescan looks like, a change the watcher cannot
/// name.
fn relevant(set: &mut WatchSet, batch: WatchBatch) -> Option<Vec<PathBuf>> {
    match batch {
        WatchBatch::Rescan => Some(Vec::new()),
        WatchBatch::Paths(paths) => {
            let relevant: Vec<PathBuf> = paths
                .into_iter()
                .filter(|path| matches!(set.classify(path), Relevance::Beamfile | Relevance::Input))
                .collect();
            (!relevant.is_empty()).then_some(relevant)
        }
    }
}

/// Folds a fresh trigger's paths into the one already held, so a burst of
/// batches during a single run announces one run over their union.
fn merge(pending: &mut Option<Vec<PathBuf>>, fresh: Vec<PathBuf>) {
    match pending {
        Some(existing) => {
            for path in fresh {
                if !existing.contains(&path) {
                    existing.push(path);
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
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut display: Vec<String> = paths
        .iter()
        .map(|path| {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            path.strip_prefix(&root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();
    display.sort();
    display.dedup();
    display
}

/// Sees the session out once its watched set could not be built. The
/// project is fixed for the session's lifetime, so rebuilding it would
/// fail identically; the loop stays alive only to end on the same terms
/// as a healthy one.
async fn idle_until_the_session_ends(
    watcher: &mut dyn Watcher,
    cancel: &CancellationToken,
) -> WatchExit {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return WatchExit::Interrupted,
            batch = watcher.next_batch() => {
                if batch.is_none() {
                    return WatchExit::WatcherClosed;
                }
            }
        }
    }
}
