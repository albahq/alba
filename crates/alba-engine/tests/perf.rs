//! Performance guard for the cache: a fully cached run must stay
//! imperceptible. The spec's criterion is "on the order of ten
//! milliseconds on a typical Beamfile, hashing included" — the fixture
//! here is 100 input files of 1 KiB, which blake3 hashes in microseconds;
//! what this actually guards is an accidental re-execution, a quadratic
//! walk, or hashing becoming per-dependent rather than per-beam.
//!
//! Median of 15 samples rather than a single run, for the same reason
//! `alba-core`'s guard does it: one sample on a loaded CI machine is
//! noise, a median is a measurement.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alba_core::{BeamId, load_str};
use alba_engine::{CacheOptions, RunOptions, run};
use alba_executors::FakeExecutor;
use tokio_util::sync::CancellationToken;

const SOURCE: &str = r#"
beam build {
  inputs ["*.txt"]
  run "work"
}
"#;

fn options(dir: &Path) -> RunOptions {
    RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: Some(CacheOptions {
            dir: dir.join(".alba").join("cache"),
            force: false,
        }),
    }
}

async fn run_once(dir: &Path) -> alba_engine::RunSummary {
    let mut project = load_str(SOURCE).expect("the fixture must load");
    for beam in &mut project.beams {
        beam.dir = dir.to_path_buf();
    }
    let (events, _incoming) = tokio::sync::mpsc::unbounded_channel();
    run(
        &project,
        &BeamId("build".to_string()),
        options(dir),
        Arc::new(FakeExecutor::new()),
        events,
        CancellationToken::new(),
    )
    .await
    .expect("the run must not fail")
}

#[tokio::test]
async fn a_fully_cached_run_stays_imperceptible() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..100 {
        std::fs::write(
            dir.path().join(format!("file_{i:03}.txt")),
            "x".repeat(1024),
        )
        .unwrap();
    }

    // Warm the manifest.
    let first = run_once(dir.path()).await;
    assert_eq!(first.succeeded.len(), 1, "the warm-up run must execute");

    let mut samples = Vec::new();
    for _ in 0..15 {
        let started = Instant::now();
        let summary = run_once(dir.path()).await;
        samples.push(started.elapsed());
        assert_eq!(summary.cached.len(), 1, "every measured run must be a hit");
    }
    samples.sort();
    let median = samples[samples.len() / 2];

    assert!(
        median < Duration::from_millis(10),
        "a fully cached 100-file run took a median of {median:?}, budget 10ms"
    );
}
