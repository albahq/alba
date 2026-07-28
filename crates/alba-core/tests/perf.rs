//! Performance guard: parsing and evaluating a Beamfile must stay roughly
//! linear in the number of beams, catching an accidental quadratic
//! regression (e.g. an O(n^2) namespace lookup) before it ships.
//!
//! Lives here, in `alba-core`, rather than as a CLI-level test that spawns
//! `alba check`: the work the criterion is actually about — parsing every
//! Beamfile, resolving imports, evaluating templates, and validating the
//! graph — happens entirely inside [`load_project`]. Spawning a process to
//! measure it would fold in process-startup cost (dynamic linker, OS
//! scheduling, first-touch filesystem caching) that has nothing to do with
//! parsing and can dwarf the sub-millisecond work under test, turning a
//! regression guard into a test that flakes for reasons unrelated to what
//! it is supposed to catch. Calling `load_project` directly, in-process,
//! measures exactly the work the criterion names and nothing else.
//!
//! The 10ms budget is calibrated for `cargo test`'s debug (unoptimized)
//! profile, since that is what every CI leg actually runs; a `--release`
//! build only has more headroom under the same threshold, never less.
//! Building the fixture (writing ~100 files to a tempdir) happens before
//! the clock starts and is never part of the measurement.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alba_core::load_project;

/// Beams declared directly in each of the 4 files in the fixture's import
/// chain (root, level1, level2, level3) — `4 * 25 = 100` beams total.
const BEAMS_PER_FILE: usize = 25;

/// Writes `content` to `path`, creating any missing parent directories
/// first — mirrors `alba-core/tests/loader.rs`'s own fixture helper.
fn write(path: PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

/// `count` trivial beams, each with a distinct id and a no-op `run`.
fn beams(count: usize) -> String {
    (0..count)
        .map(|i| format!("beam b{i} {{ run \"echo {i}\" }}\n"))
        .collect()
}

/// Lays out a 3-level import chain — root imports level1, which imports
/// level2, which imports level3 — with `BEAMS_PER_FILE` beams declared in
/// each of the 4 files, and returns the root Beamfile's path.
fn write_fixture(dir: &std::path::Path) -> PathBuf {
    let root = dir.join("Beamfile");
    write(
        root.clone(),
        &format!(
            "import \"level1/Beamfile\" as l1\n{}",
            beams(BEAMS_PER_FILE)
        ),
    );
    write(
        dir.join("level1/Beamfile"),
        &format!(
            "import \"level2/Beamfile\" as l2\n{}",
            beams(BEAMS_PER_FILE)
        ),
    );
    write(
        dir.join("level1/level2/Beamfile"),
        &format!(
            "import \"level3/Beamfile\" as l3\n{}",
            beams(BEAMS_PER_FILE)
        ),
    );
    write(
        dir.join("level1/level2/level3/Beamfile"),
        &beams(BEAMS_PER_FILE),
    );
    root
}

#[test]
fn load_project_stays_under_10ms_of_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = write_fixture(dir.path());

    // One untimed load first, so the timed runs measure parsing and
    // evaluation rather than a cold filesystem cache warming up.
    load_project(&root).unwrap();

    let mut samples: Vec<Duration> = (0..10)
        .map(|_| {
            let start = Instant::now();
            let (project, _sources) = load_project(&root).unwrap();
            let elapsed = start.elapsed();
            assert_eq!(project.beams.len(), BEAMS_PER_FILE * 4);
            elapsed
        })
        .collect();

    samples.sort();
    let median = samples[samples.len() / 2];

    assert!(
        median < Duration::from_millis(10),
        "median load_project() time over 10 runs was {median:?}, expected under 10ms \
         (all samples: {samples:?})"
    );
}
