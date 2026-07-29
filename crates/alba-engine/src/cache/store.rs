//! Persistence for the cache: one JSON manifest and one JSONL log file per
//! beam, under the project's `.alba/cache/` directory.
//!
//! Everything here is best-effort by design — the cache must never fail a
//! run. A manifest that cannot be read, parsed, or trusted (wrong format
//! version) is a miss; a write that fails is silently dropped and the old
//! entry, if any, stays in place. Writes are atomic (temporary file +
//! rename) so a concurrent `alba run` in the same project can at worst
//! overwrite an entry, never tear one.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use alba_core::BeamId;
use alba_executors::{OutputLine, Stream};
use serde::{Deserialize, Serialize};

/// Bumped whenever the manifest layout *or the fingerprint recipe*
/// changes incompatibly: a manifest from another version is a miss, which
/// re-runs the beam and rewrites the entry — no migration, ever.
pub(crate) const FORMAT_VERSION: u32 = 2;

/// What the last successful run of a beam left behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    pub fingerprint: String,
    /// The rendered `outputs` patterns at the time of that run. Currently
    /// informational (the decision checks the beam's *current*
    /// declaration); stored so a future store-and-restore can trust it.
    pub outputs: Vec<String>,
    /// How long the run took, replayed as the cached beam's duration.
    pub duration_ms: u64,
}

/// One stored log line. A type of this module's own rather than a serde
/// derive on [`OutputLine`]: the on-disk format is a contract this file
/// owns, not a reflection of another crate's internal layout.
#[derive(Serialize, Deserialize)]
struct StoredLine {
    stream: String,
    text: String,
}

pub(crate) struct CacheStore {
    dir: PathBuf,
}

impl CacheStore {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn load(&self, id: &BeamId) -> Option<Manifest> {
        let bytes = std::fs::read(self.entry(id, "json")).ok()?;
        let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
        (manifest.version == FORMAT_VERSION).then_some(manifest)
    }

    pub(crate) fn store(&self, id: &BeamId, manifest: &Manifest, logs: &[OutputLine]) {
        // Best-effort: an unwritable cache degrades to re-running the
        // beam next time, which is always safe.
        let _ = self.try_store(id, manifest, logs);
    }

    /// The other half of the round trip: the scheduler stores logs on a
    /// successful run and reads them back here once a cache hit replays
    /// that run's output.
    pub(crate) fn load_logs(&self, id: &BeamId) -> Vec<OutputLine> {
        let Ok(content) = std::fs::read_to_string(self.entry(id, "log")) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|line| serde_json::from_str::<StoredLine>(line).ok())
            .map(|line| OutputLine {
                stream: match line.stream.as_str() {
                    "stderr" => Stream::Stderr,
                    _ => Stream::Stdout,
                },
                text: line.text,
            })
            .collect()
    }

    fn try_store(&self, id: &BeamId, manifest: &Manifest, logs: &[OutputLine]) -> io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;

        let mut log_lines = String::new();
        for line in logs {
            let stored = StoredLine {
                stream: match line.stream {
                    Stream::Stdout => "stdout".to_string(),
                    Stream::Stderr => "stderr".to_string(),
                },
                text: line.text.clone(),
            };
            // Serializing two strings cannot fail; skip defensively anyway.
            if let Ok(json) = serde_json::to_string(&stored) {
                log_lines.push_str(&json);
                log_lines.push('\n');
            }
        }
        // Logs first, manifest last: a crash in between leaves the old
        // manifest (a stale but consistent hit) rather than a new
        // manifest pointing at logs that were never written.
        write_atomic(&self.entry(id, "log"), log_lines.as_bytes())?;
        let bytes = serde_json::to_vec_pretty(manifest)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_atomic(&self.entry(id, "json"), &bytes)
    }

    /// Entries are named by a hash of the beam id: ids contain `:` (not a
    /// legal filename character on windows) and arbitrary user text.
    fn entry(&self, id: &BeamId, extension: &str) -> PathBuf {
        let name = blake3::hash(id.0.as_bytes()).to_hex();
        self.dir.join(format!("{}.{extension}", &name[..32]))
    }
}

/// Writes via a sibling temporary file and a rename, so a reader never
/// observes a half-written entry.
///
/// The temporary name appends to the full file name (rather than replacing
/// the extension) so the `.json` and `.log` of one beam cannot collide on
/// the same temporary path, and carries the process id and a counter so
/// two concurrent `alba run` invocations writing the same beam cannot
/// either — without which they would share one temporary file and each
/// rename whatever the other had written into it half-way.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = temporary_path(path);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// A temporary sibling of `path`, never the same one twice.
fn temporary_path(path: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alba_executors::Stream;

    fn store_in(dir: &std::path::Path) -> CacheStore {
        CacheStore::new(dir.join("cache"))
    }

    fn manifest(fingerprint: &str) -> Manifest {
        Manifest {
            version: FORMAT_VERSION,
            fingerprint: fingerprint.to_string(),
            outputs: vec!["out.txt".to_string()],
            duration_ms: 1234,
        }
    }

    fn line(stream: Stream, text: &str) -> OutputLine {
        OutputLine {
            stream,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_stored_manifest_loads_back_identically() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let id = BeamId("build".to_string());

        store.store(&id, &manifest("fp1"), &[]);

        assert_eq!(store.load(&id), Some(manifest("fp1")));
    }

    #[test]
    fn a_beam_never_stored_loads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            store_in(dir.path()).load(&BeamId("build".to_string())),
            None
        );
    }

    /// Namespaced ids contain `:`, which is not a legal filename character
    /// on windows — entries are stored under a hash of the id, so any id
    /// is safe and two ids never collide on disk.
    #[test]
    fn namespaced_ids_are_stored_safely_and_separately() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let plain = BeamId("build".to_string());
        let namespaced = BeamId("api:build".to_string());

        store.store(&plain, &manifest("fp-plain"), &[]);
        store.store(&namespaced, &manifest("fp-ns"), &[]);

        assert_eq!(store.load(&plain).unwrap().fingerprint, "fp-plain");
        assert_eq!(store.load(&namespaced).unwrap().fingerprint, "fp-ns");
    }

    #[test]
    fn a_corrupted_manifest_is_a_miss_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let id = BeamId("build".to_string());
        store.store(&id, &manifest("fp1"), &[]);

        // Corrupt every manifest on disk; the store must shrug it off.
        for entry in std::fs::read_dir(dir.path().join("cache")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                std::fs::write(&path, "not json at all").unwrap();
            }
        }

        assert_eq!(store.load(&id), None);
    }

    #[test]
    fn a_manifest_from_another_format_version_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let id = BeamId("build".to_string());

        let old = Manifest {
            version: FORMAT_VERSION + 1,
            ..manifest("fp1")
        };
        store.store(&id, &old, &[]);

        assert_eq!(store.load(&id), None);
    }

    #[test]
    fn logs_roundtrip_with_stream_and_order_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let id = BeamId("build".to_string());
        let logs = vec![
            line(Stream::Stdout, "compiling"),
            line(Stream::Stderr, "warning: unused"),
            line(Stream::Stdout, "done"),
        ];

        store.store(&id, &manifest("fp1"), &logs);

        assert_eq!(store.load_logs(&id), logs);
    }

    /// Two writers of the same entry must not share a temporary file:
    /// they would each rename whatever the other had written half-way
    /// into it, which is precisely the torn entry the rename is there to
    /// prevent. Concurrent `alba run` invocations in one project are
    /// tolerated, so this is a real pair of writers, not a hypothetical.
    #[test]
    fn two_writes_of_the_same_entry_use_different_temporary_files() {
        let path = Path::new("/cache/abc.json");

        let (first, second) = (temporary_path(path), temporary_path(path));

        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(Path::new("/cache")));
    }

    #[test]
    fn missing_logs_load_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            store_in(dir.path())
                .load_logs(&BeamId("build".to_string()))
                .is_empty()
        );
    }
}
