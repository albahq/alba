//! Watch mode: re-run the target when its declared `inputs` change.
//! `set` decides which changed paths matter; the session loop arrives
//! with the rest of the module.

mod set;

pub(crate) use set::{Relevance, WatchSet};
