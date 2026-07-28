//! The skip-only cache: fingerprint a ready beam, decide hit or miss, and
//! persist the last successful run's manifest and logs under
//! `.alba/cache/`. Sub-modules: `fingerprint` (hashing), `store`
//! (persistence). The scheduler is the only consumer.

use std::path::PathBuf;

mod fingerprint;
mod store;

pub(crate) use fingerprint::{BeamFacts, fingerprint, hash_file, static_contribution};
pub(crate) use store::{CacheStore, FORMAT_VERSION, Manifest};

/// Caller-facing cache configuration: where the state lives and whether
/// this run ignores it when reading. `force` still *writes*: a forced run
/// rewrites every successful beam's entry.
#[derive(Debug, Clone)]
pub struct CacheOptions {
    pub dir: PathBuf,
    pub force: bool,
}
