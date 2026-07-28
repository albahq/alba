//! Performance guard: parsing, resolving imports, and validating the
//! dependency graph must stay roughly linear in the number of beams and
//! `needs` edges, catching an accidental quadratic regression (e.g. an
//! O(n^2) namespace lookup while resolving imports, or an O(n^2) id lookup
//! while validating `needs`) before it ships.
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
//! The fixture is not just 100 beams with empty `needs` lists. An earlier
//! version of this test was exactly that, and a review caught that it
//! exercised only the parsing/import half of `load_project`:
//! `validate_graph`'s `by_id` lookup (see `graph.rs`) never iterates a
//! `needs` entry when there are none, so an O(1)-to-O(n) regression there
//! was completely invisible to it — confirmed by deliberately breaking
//! that lookup into a linear scan and watching the measured median not
//! move at all. Every beam declared here now needs every beam declared
//! before it in the same file (a triangular fan-in, the densest DAG a
//! file's beams can form without a cycle), and each file that imports
//! another additionally needs that import's last beam through its alias,
//! so the cross-namespace `needs` lookup path is exercised too.
//!
//! Doing the same mutation against *this* fixture does move the measured
//! time — repeatably, not as noise — but only by roughly 10-20%, not the
//! multiple-times jump the parsing-side mutation produces (see the task
//! report this test's introduction was reviewed under for both sets of
//! numbers). That gap is real, not a weaker fixture: at 100 beams, a
//! linear scan over short ids is simply cheap in absolute terms (most
//! candidates are rejected by a length check before ever comparing
//! bytes), so an O(1)-vs-O(n) difference here is inherently a smaller
//! fraction of the total than the parsing-side regression is. Pushing the
//! fixture denser to force a larger gap was tried and rejected: it made
//! *this* test's own baseline (the correct implementation) approach the
//! 10ms budget on this machine, which would trade a real flake risk for a
//! bigger number in a comment. Catching a regression at all — clearly and
//! repeatably above run-to-run noise — is what this fixture is for; it is
//! not a benchmark of how bad a hypothetical regression could be made to
//! look.
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

/// `count` beams with real dependency edges, not just distinct ids: beam
/// `bI` needs every beam declared before it in this same file
/// (`b0..b(I-1)`, a triangular fan-in — `I` edges for beam `I`, `O(count^2)`
/// edges overall), plus `cross_need` on `b0` when given — a namespaced
/// reference into whichever file this one imports, so the cross-file,
/// alias-prefixed `needs` lookup path is exercised too, not only the
/// plain one. None of this can create a cycle: every intra-file edge
/// points strictly backwards by index, and a cross-file edge points into
/// a file that has already finished loading and cannot depend back on
/// this one.
fn beams(count: usize, cross_need: Option<&str>) -> String {
    let mut out = String::new();
    for i in 0..count {
        let mut needs: Vec<String> = (0..i).map(|j| format!("b{j}")).collect();
        if i == 0 {
            needs.extend(cross_need.map(str::to_string));
        }
        let clause = if needs.is_empty() {
            String::new()
        } else {
            format!(" needs [{}]", needs.join(", "))
        };
        out.push_str(&format!("beam b{i} {{{clause} run \"echo {i}\" }}\n"));
    }
    out
}

/// Lays out a 3-level import chain — root imports level1, which imports
/// level2, which imports level3 — with `BEAMS_PER_FILE` beams declared in
/// each of the 4 files, and returns the root Beamfile's path. Each file's
/// first beam additionally needs the last beam its import declares (the
/// one every other beam in that imported file already transitively needs,
/// being its triangular fan-in's final entry), so real dependency edges
/// run across the whole tree, not just within each file.
fn write_fixture(dir: &std::path::Path) -> PathBuf {
    let last = BEAMS_PER_FILE - 1;
    let root = dir.join("Beamfile");
    write(
        root.clone(),
        &format!(
            "import \"level1/Beamfile\" as l1\n{}",
            beams(BEAMS_PER_FILE, Some(&format!("l1:b{last}")))
        ),
    );
    write(
        dir.join("level1/Beamfile"),
        &format!(
            "import \"level2/Beamfile\" as l2\n{}",
            beams(BEAMS_PER_FILE, Some(&format!("l2:b{last}")))
        ),
    );
    write(
        dir.join("level1/level2/Beamfile"),
        &format!(
            "import \"level3/Beamfile\" as l3\n{}",
            beams(BEAMS_PER_FILE, Some(&format!("l3:b{last}")))
        ),
    );
    write(
        dir.join("level1/level2/level3/Beamfile"),
        &beams(BEAMS_PER_FILE, None),
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
