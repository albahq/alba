//! Persistence for the cache: one JSON manifest and one JSONL log file per
//! beam, under the project's `.alba/cache/` directory.
//!
//! Everything here is best-effort by design — the cache must never fail a
//! run. A manifest that cannot be read, parsed, or trusted (wrong format
//! version) is a miss; a write that fails is silently dropped and the old
//! entry, if any, stays in place. Writes are atomic (temporary file +
//! rename) so a concurrent `alba run` in the same project can at worst
//! overwrite an entry, never tear one.
//!
//! The logs hold whatever a beam's commands printed, secrets included when
//! a command prints one, so on unix the directory is private to its owner
//! (`0700`) and every entry is written `0600`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use alba_core::BeamId;
use alba_executors::{OutputLine, Stream};
use serde::{Deserialize, Serialize};

/// Bumped whenever the manifest layout *or the fingerprint recipe*
/// changes incompatibly: a manifest from another version is a miss, which
/// re-runs the beam and rewrites the entry — no migration, ever.
pub(crate) const FORMAT_VERSION: u32 = 3;

/// The Alba that writes and reads entries: an entry another version wrote
/// is a miss, like one in another format.
pub(crate) const ALBA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the last successful run of a beam left behind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    pub alba_version: String,
    pub fingerprint: String,
    /// The rendered `outputs` patterns at the time of that run. Currently
    /// informational (the decision checks the beam's *current*
    /// declaration); stored so a future store-and-restore can trust it.
    pub outputs: Vec<String>,
    /// How long the run took, replayed as the cached beam's duration.
    pub duration_ms: u64,
    /// Every file the run left under `outputs`, as it left it: a hit
    /// requires the outputs on disk to still match, file for file.
    pub output_files: Vec<OutputFile>,
}

/// One output file as a run left it. Size and modification time rather
/// than a content hash: outputs are build artifacts, often large and
/// numerous (`dist/`, a release binary), and reading them whole on every
/// run would cost more than the beam the cache is skipping. Anything that
/// rewrites a file moves its modification time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OutputFile {
    pub path: String,
    pub size: u64,
    /// Nanoseconds since the unix epoch; `None` on a filesystem that does
    /// not report one, where the size alone is compared.
    pub modified_ns: Option<u64>,
}

impl OutputFile {
    /// What `outputs` name on disk right now under `dir`, in the form a
    /// manifest stores. A file that vanishes while being listed is left
    /// out, which can only turn a hit into a miss.
    pub(crate) fn current(dir: &Path, outputs: &[String]) -> Vec<OutputFile> {
        alba_core::output_files(dir, outputs)
            .into_iter()
            .filter_map(|(path, absolute)| {
                let metadata = std::fs::metadata(absolute).ok()?;
                let modified_ns = metadata
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|since| u64::try_from(since.as_nanos()).ok());
                Some(OutputFile {
                    path,
                    size: metadata.len(),
                    modified_ns,
                })
            })
            .collect()
    }
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
        (manifest.version == FORMAT_VERSION && manifest.alba_version == ALBA_VERSION)
            .then_some(manifest)
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
        create_private_dir(&self.dir)?;

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
    write_private(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Creates `dir` if needed and makes it private to its owner, including a
/// directory an earlier version created world-readable.
fn create_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes a new file readable by its owner only, from its creation on:
/// there is no window in which it exists with wider permissions.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
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
            alba_version: ALBA_VERSION.to_string(),
            fingerprint: fingerprint.to_string(),
            outputs: vec!["out.txt".to_string()],
            duration_ms: 1234,
            output_files: vec![OutputFile {
                path: "out.txt".to_string(),
                size: 8,
                modified_ns: Some(1),
            }],
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

    /// Another Alba may run a beam differently (the embedded shell's
    /// builtins, how commands are rendered): its entries are not trusted.
    #[test]
    fn a_manifest_written_by_another_alba_version_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let id = BeamId("build".to_string());

        let other = Manifest {
            alba_version: "0.0.0-other".to_string(),
            ..manifest("fp1")
        };
        store.store(&id, &other, &[]);

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
