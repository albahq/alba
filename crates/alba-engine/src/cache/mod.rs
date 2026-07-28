//! The skip-only cache: fingerprint a ready beam, decide hit or miss, and
//! persist the last successful run's manifest and logs under
//! `.alba/cache/`. Sub-modules: `fingerprint` (hashing), `store`
//! (persistence, Task 3). The scheduler is the only consumer.

mod fingerprint;

#[allow(dead_code, unused_imports)] // consumed by the scheduler in a follow-up change
pub(crate) use fingerprint::{BeamFacts, fingerprint, hash_file, static_contribution};
