//! The real [`Watcher`]: `notify` behind `notify-debouncer-full`.
//!
//! The debouncer runs its own thread and calls the handler with each
//! coalesced batch; the handler forwards into an unbounded channel the
//! async side reads. The debouncer must stay alive as long as the
//! watcher — dropping it silently stops all delivery — so it rides along
//! in the struct. Watcher errors (overflow included) become
//! [`WatchBatch::Rescan`]: the loop treats "something changed but I
//! cannot say what" as a trigger and lets the cache absorb the
//! imprecision.

use std::path::PathBuf;
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_full::{Debouncer, RecommendedCache};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{WatchBatch, Watcher};

pub struct NotifyWatcher {
    batches: UnboundedReceiver<WatchBatch>,
    // Kept alive only for its `Drop`: dropping the debouncer stops the
    // background thread that feeds `batches`, so this field is never
    // read, only held.
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

impl NotifyWatcher {
    /// The built-in debounce window: long enough to coalesce an editor's
    /// save burst or a `git checkout`, short enough to feel immediate.
    pub const DEBOUNCE: Duration = Duration::from_millis(200);

    /// Starts watching every root recursively. An error here is fatal to
    /// the session before it starts — the CLI turns it into exit code 2.
    pub fn new(roots: &[PathBuf]) -> Result<Self, notify::Error> {
        let (sender, batches) = unbounded_channel();
        let mut debouncer = notify_debouncer_full::new_debouncer(
            Self::DEBOUNCE,
            None,
            move |result: notify_debouncer_full::DebounceEventResult| {
                let batch = match result {
                    Ok(events) => WatchBatch::Paths(
                        events
                            .into_iter()
                            .flat_map(|event| event.paths.clone())
                            .collect(),
                    ),
                    Err(_) => WatchBatch::Rescan,
                };
                // A send failure means the session is gone; nothing to do.
                let _ = sender.send(batch);
            },
        )?;
        for root in roots {
            debouncer.watch(root, RecursiveMode::Recursive)?;
        }
        Ok(Self {
            batches,
            _debouncer: debouncer,
        })
    }
}

#[async_trait::async_trait]
impl Watcher for NotifyWatcher {
    async fn next_batch(&mut self) -> Option<WatchBatch> {
        self.batches.recv().await
    }
}
