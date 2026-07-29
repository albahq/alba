# Alba Watch Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `alba run --watch`: run the target beam, then re-run it whenever the files its subgraph declares as `inputs` change, with cancel-and-restart semantics, hot Beamfile reload, and watch events on the existing event channel.

**Architecture:** A `watch` module inside `alba-engine`, next to `cache`, owns the session loop: load, plan, run through the existing scheduler (cache active), wait for debounced file changes, cancel-and-restart. File watching is `notify` + `notify-debouncer-full` behind a small internal `Watcher` trait so engine tests inject synthetic batches. The event channel gains two watch variants consumed by all three renderers. Spec: `.claude/superpowers/specs/2026-07-29-alba-watch-design.md`.

**Tech Stack:** Rust (edition 2024, workspace), tokio, tokio-util (CancellationToken), notify + notify-debouncer-full (file watching), globset (event matching), async-trait, assert_cmd + tempfile (tests), nix (unix signalling in e2e tests).

## Global Constraints

- TDD everywhere: write the failing test first, watch it fail, implement, watch it pass, commit.
- Workspace lints stay green at every commit: `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
- Commit messages: gitmoji + Conventional Commits (`✨ feat(engine): ...`), English, no attribution trailers, never referencing this plan or its task numbers.
- Dependency rules unchanged: `cli → engine → core → syntax`; the engine sees executors only through the `Executor` trait; the event channel stays the only contract between execution and display.
- Cross-platform: everything compiles and passes on macOS, Linux, and Windows. Unix-only test code goes behind `#[cfg(unix)]` like the existing interrupt tests.
- Debounce is a built-in constant (200 ms), not a flag.
- Exit codes: a watch session ends 0 on Ctrl-C; 2 stays reserved for Alba errors at startup (unloadable Beamfile, watcher unavailable).
- Comments follow the existing house style: explain constraints and intent, never restate the code.

## Spec-to-plan mapping (read first)

The spec names four watch event kinds ("session started, waiting for changes, run triggered, run interrupted"). This plan implements them as **two** `RunEvent` variants, deliberately:

- `WatchWaiting { files }` — emitted after each run when the loop starts waiting. The first one doubles as "session started" (it carries the watched-file count).
- `WatchTriggered { paths }` — emitted when relevant changes start a new run. When the change landed mid-run, the loop cancels the run, waits for its cancelled summary, and *then* emits `WatchTriggered`; its position after a summary full of `cancelled` beams is what marks that run as "interrupted by the watch", and the text renderers phrase it that way. An empty `paths` means a watcher overflow (rescan): renderers print a generic "changes detected".

## File structure

New files:

- `crates/alba-engine/src/watch/mod.rs` — `Watcher` trait, `WatchBatch`, `WatchExit`, `SessionError`, the session loop `watch()`.
- `crates/alba-engine/src/watch/set.rs` — `WatchSet`: which paths are relevant (inputs patterns per beam directory, loaded Beamfiles, git-aware filtering).
- `crates/alba-engine/src/watch/notify.rs` — `NotifyWatcher`, the real `Watcher` implementation.
- `crates/alba-engine/tests/watch.rs` — loop behaviour tests against `FakeExecutor` and a scripted watcher.
- `crates/alba-cli/tests/cli_watch.rs` — end-to-end tests on a real long-running `alba run --watch` process.

Modified files:

- `crates/alba-core/src/loader.rs` — `SourceMap::paths()`.
- `crates/alba-engine/src/event.rs` — two watch variants on `RunEvent`.
- `crates/alba-engine/src/lib.rs` — export the watch API.
- `crates/alba-engine/Cargo.toml` — add `globset`, `async-trait`, `notify`, `notify-debouncer-full`.
- `crates/alba-cli/src/render/{mod,interleaved,grouped,json}.rs` — render the watch variants; `LineSink::raw`; clear-screen support.
- `crates/alba-cli/src/args.rs` — `--watch` flag; `beam` becomes optional on `alba run`.
- `crates/alba-cli/src/main.rs` — resolve an omitted beam to the declared `default`; make `render_load_error` reachable from `commands::run`.
- `crates/alba-cli/src/commands/run.rs` — the watch execution path.
- `README.md` — document watch mode.

---

### Task 1: `SourceMap::paths()`

The watch loop must know every loaded Beamfile (root and imports) to watch them. `SourceMap` already stores each file's path but only exposes lookup by `SourceId`.

**Files:**
- Modify: `crates/alba-core/src/loader.rs`
- Modify: `crates/alba-core/Cargo.toml` (dev-dependency `tempfile`, workspace version, if not already present)

**Interfaces:**
- Consumes: the existing `SourceMap { entries: Vec<(PathBuf, String)> }`.
- Produces: `pub fn paths(&self) -> impl Iterator<Item = &Path>` on `SourceMap`, yielding paths in `SourceId` order (root first, then imports in load order).

- [ ] **Step 1: Write the failing test**

In the `#[cfg(test)]` module of `crates/alba-core/src/loader.rs` (create one at the bottom of the file if none exists):

```rust
/// The watch loop needs every loaded Beamfile path — root and imports —
/// to put them under watch; `paths()` yields them in load order.
#[test]
fn source_map_lists_every_loaded_file_in_load_order() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("api")).unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "import \"api/Beamfile\" as api\nbeam build { run \"echo root\" }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("api").join("Beamfile"),
        "beam build { run \"echo api\" }\n",
    )
    .unwrap();

    let (_, sources) = load_project(&dir.path().join("Beamfile")).unwrap();

    let paths: Vec<_> = sources.paths().collect();
    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("Beamfile"));
    assert!(paths[1].ends_with(std::path::Path::new("api").join("Beamfile")));
}
```

If `loader.rs` already has a test module, add the test there and match its existing imports/helpers instead of duplicating them.

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test -p alba-core source_map_lists_every_loaded_file_in_load_order`
Expected: compile error, `paths` not found on `SourceMap`.

- [ ] **Step 3: Implement**

In `impl SourceMap`, next to `get`:

```rust
/// The path of every file this map registered, in [`SourceId`] order:
/// the root Beamfile first, then each import in load order. This is the
/// watch loop's source of truth for which Beamfiles to put under watch.
pub fn paths(&self) -> impl Iterator<Item = &Path> {
    self.entries.iter().map(|(path, _)| path.as_path())
}
```

- [ ] **Step 4: Run the test and the crate's suite**

Run: `cargo test -p alba-core`
Expected: PASS.

- [ ] **Step 5: Lints, then commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-core
git commit -m "✨ feat(core): let SourceMap list its loaded file paths"
```

---

### Task 2: Watch events on the run stream, rendered by all three renderers

Two new `RunEvent` variants, plus their rendering. Adding variants breaks the renderers' exhaustive matches, so event and renderers change together to keep the workspace compiling.

**Files:**
- Modify: `crates/alba-engine/src/event.rs`
- Modify: `crates/alba-cli/src/render/mod.rs` (shared text helper, `LineSink::raw`)
- Modify: `crates/alba-cli/src/render/interleaved.rs`, `crates/alba-cli/src/render/grouped.rs` (constructor gains `clear_between_runs`), `crates/alba-cli/src/render/json.rs`
- Modify: `crates/alba-cli/src/commands/run.rs` (existing constructor call sites pass `false`)

**Interfaces:**
- Produces: `RunEvent::WatchWaiting { files: usize }` and `RunEvent::WatchTriggered { paths: Vec<String> }` (paths are project-root-relative display strings with `/` separators; empty vector = watcher rescan).
- Produces: `InterleavedRenderer::new(color: bool, clear_between_runs: bool)`, `GroupedRenderer::new(clear_between_runs: bool)`, `LineSink::raw(&mut self, text: &str)` (write without newline, flush, same latch-on-failure behaviour as `line`).
- Produces: JSON wire events `{"event":"watch_waiting","files":N}` and `{"event":"watch_triggered","paths":[...]}`.

- [ ] **Step 1: Write the failing tests**

In the test module of `crates/alba-cli/src/render/json.rs` (or create one following the file's conventions), test the wire shape via `WireEvent::from` + `serde_json::to_string`, matching however the existing wire tests are written (look for them first — `grep -n "mod tests" crates/alba-cli/src/render/*.rs`). If no wire tests exist, add:

```rust
#[test]
fn watch_events_serialize_with_their_own_tags() {
    let waiting = RunEvent::WatchWaiting { files: 42 };
    let triggered = RunEvent::WatchTriggered {
        paths: vec!["src/lib.rs".to_string()],
    };

    let waiting = serde_json::to_string(&WireEvent::from(&waiting)).unwrap();
    let triggered = serde_json::to_string(&WireEvent::from(&triggered)).unwrap();

    assert_eq!(waiting, r#"{"event":"watch_waiting","files":42}"#);
    assert_eq!(
        triggered,
        r#"{"event":"watch_triggered","paths":["src/lib.rs"]}"#
    );
}
```

In `crates/alba-cli/src/render/mod.rs`, test the shared text lines (the helper the two text renderers will call):

```rust
#[test]
fn watch_lines_read_as_status_not_as_beam_output() {
    assert_eq!(
        watch_line(&RunEvent::WatchWaiting { files: 3 }),
        Some("watching — 3 files, waiting for changes".to_string())
    );
    assert_eq!(
        watch_line(&RunEvent::WatchTriggered {
            paths: vec!["a.rs".into(), "b.rs".into()],
        }),
        Some("change detected in a.rs, b.rs — running".to_string())
    );
    // Beyond three paths, name the first three and count the rest.
    assert_eq!(
        watch_line(&RunEvent::WatchTriggered {
            paths: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
        }),
        Some("change detected in a, b, c and 2 more — running".to_string())
    );
    // A rescan has no paths to name.
    assert_eq!(
        watch_line(&RunEvent::WatchTriggered { paths: vec![] }),
        Some("changes detected — running".to_string())
    );
    assert_eq!(watch_line(&RunEvent::RunFinished { summary: RunSummary::default() }), None);
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p alba-cli watch_`
Expected: compile errors (missing variants, missing `watch_line`).

- [ ] **Step 3: Add the variants**

In `crates/alba-engine/src/event.rs`, extend `RunEvent` and its doc comment:

```rust
    /// Watch mode only: the session finished a run and is now waiting.
    /// `files` is how many files the watched `inputs` currently resolve
    /// to — the status line's number. The first one a session emits also
    /// announces the session itself.
    WatchWaiting {
        files: usize,
    },
    /// Watch mode only: relevant changes started a new run. `paths` are
    /// project-root-relative display strings; empty means the watcher
    /// overflowed and could not say what changed (rescan). Emitted after
    /// the previous run's `RunFinished` — when that summary is full of
    /// cancelled beams, this event is what marks the run as interrupted
    /// by the watch rather than abandoned by the user.
    WatchTriggered {
        paths: Vec<String>,
    },
```

Also update the `RunEvent` enum's top doc comment: `RunFinished` is last *per run*; in a watch session, watch variants sit between runs.

- [ ] **Step 4: Render them**

In `crates/alba-cli/src/render/mod.rs`:

```rust
/// The status line a watch event renders to, `None` for every other
/// event. Shared by the two text renderers so their phrasing cannot
/// drift; both print it to stderr — it is Alba talking about the run,
/// not the run's output (see the module doc comment).
pub(super) fn watch_line(event: &RunEvent) -> Option<String> {
    match event {
        RunEvent::WatchWaiting { files } => {
            Some(format!("watching — {files} files, waiting for changes"))
        }
        RunEvent::WatchTriggered { paths } if paths.is_empty() => {
            Some("changes detected — running".to_string())
        }
        RunEvent::WatchTriggered { paths } => {
            let named = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            let rest = paths.len().saturating_sub(3);
            if rest == 0 {
                Some(format!("change detected in {named} — running"))
            } else {
                Some(format!("change detected in {named} and {rest} more — running"))
            }
        }
        _ => None,
    }
}
```

Add `raw` to `LineSink`, next to `line`:

```rust
/// Writes `text` exactly as given — no newline — and flushes, or does
/// nothing once the sink is closed. Exists for the clear-screen escape
/// sequence, which must not be followed by a newline and must reach the
/// terminal before the next run's first line.
pub fn raw(&mut self, text: &str) {
    if !self.open {
        return;
    }
    if write!(self.writer, "{text}")
        .and_then(|()| self.writer.flush())
        .is_err()
    {
        self.open = false;
    }
}
```

In `interleaved.rs` and `grouped.rs`:

- Change the constructors to `InterleavedRenderer::new(color: bool, clear_between_runs: bool)` and `GroupedRenderer::new(clear_between_runs: bool)`, storing the flag.
- At the top of each `handle`, before the existing match:

```rust
if let Some(line) = super::watch_line(event) {
    if matches!(event, RunEvent::WatchTriggered { .. }) && self.clear_between_runs {
        // \x1b[2J clears the screen, \x1b[3J the scrollback, \x1b[H homes
        // the cursor: each triggered run starts on a clean page.
        self.out.raw("\u{1b}[2J\u{1b}[3J\u{1b}[H");
    }
    self.err.line(&line);
    return;
}
```

Adapt the field names to each renderer's actual `LineSink` fields (read the files first: the stdout sink and stderr sink may be named differently, and `GroupedRenderer` may create its stderr sink on demand — add a stored one if needed). The existing exhaustive `match event` arms must then ignore the two new variants (`RunEvent::WatchWaiting { .. } | RunEvent::WatchTriggered { .. } => {}`) since `watch_line` already handled them and returned.

In `json.rs`, extend `WireEvent` and its `From`:

```rust
    WatchWaiting {
        files: usize,
    },
    WatchTriggered {
        paths: &'a [String],
    },
```

```rust
            RunEvent::WatchWaiting { files } => WireEvent::WatchWaiting { files: *files },
            RunEvent::WatchTriggered { paths } => WireEvent::WatchTriggered { paths },
```

In `crates/alba-cli/src/commands/run.rs`, update the two constructor call sites in `renderer()` to pass `false` for `clear_between_runs` (the watch path sets it in Task 8).

- [ ] **Step 5: Run the tests and the workspace**

Run: `cargo test -p alba-cli && cargo test --workspace`
Expected: PASS (existing renderer tests still green).

- [ ] **Step 6: Lints, then commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-engine crates/alba-cli
git commit -m "✨ feat(engine): add watch events to the run stream"
```

---

### Task 3: `WatchSet` — which changed paths are relevant

The filter at the heart of the loop: a changed path is relevant if it is a loaded Beamfile, or if it matches a subgraph beam's `inputs` pattern *and* is not git-ignored. Git-awareness is delegated to the existing `expand_globs` (the same function the cache uses), so watch and cache can never disagree about what an input is.

**Files:**
- Create: `crates/alba-engine/src/watch/set.rs`
- Create: `crates/alba-engine/src/watch/mod.rs` (for now only `mod set;` and the re-exports Task 4 will grow)
- Modify: `crates/alba-engine/src/lib.rs` (add `mod watch;`)
- Modify: `crates/alba-engine/Cargo.toml` (add `globset = { workspace = true }`; dev-dependency `tempfile` if absent)

**Interfaces:**
- Consumes: `alba_core::{execution_subgraph, expand_globs, Project, BeamId, SourceMap, CoreError}`; `Beam::{dir, inputs}`; `SourceMap::paths()` (Task 1).
- Produces (crate-private, used by Task 4's loop):

```rust
pub(crate) struct WatchSet { /* fields below */ }
pub(crate) enum Relevance { Beamfile, Input, Irrelevant }
impl WatchSet {
    pub(crate) fn new(project: &Project, target: &BeamId, sources: &SourceMap) -> Result<Self, CoreError>;
    pub(crate) fn file_count(&self) -> usize;
    pub(crate) fn classify(&mut self, path: &Path) -> Relevance;
}
```

- [ ] **Step 1: Write the failing tests**

`crates/alba-engine/src/watch/set.rs`, with its test module at the bottom. Tests use a real temporary project so `expand_globs` and `.gitignore` behave exactly as in production:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::{BeamId, load_project};

    /// A real project on disk: a Beamfile whose `build` beam watches
    /// `src/**/*.rs`, one matching source file, one git-ignored artifact
    /// directory that also matches the pattern shape.
    fn project_on_disk() -> (tempfile::TempDir, WatchSet) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("target/gen.rs"), "generated").unwrap();
        std::fs::write(
            dir.path().join("Beamfile"),
            "beam build { inputs [\"src/**/*.rs\"] run \"echo build\" }\n\
             beam free { run \"echo free\" }\n",
        )
        .unwrap();

        let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
        let set = WatchSet::new(&project, &BeamId("build".to_string()), &sources).unwrap();
        (dir, set)
    }

    #[test]
    fn a_loaded_beamfile_classifies_as_beamfile() {
        let (dir, mut set) = project_on_disk();
        assert!(matches!(
            set.classify(&dir.path().join("Beamfile")),
            Relevance::Beamfile
        ));
    }

    #[test]
    fn a_matching_source_file_classifies_as_input() {
        let (dir, mut set) = project_on_disk();
        assert!(matches!(
            set.classify(&dir.path().join("src/lib.rs")),
            Relevance::Input
        ));
    }

    /// The typical feedback-loop path: a build artifact whose name shape
    /// matches the pattern but that `.gitignore` declares uninteresting.
    #[test]
    fn a_gitignored_file_never_triggers() {
        let (dir, mut set) = project_on_disk();
        assert!(matches!(
            set.classify(&dir.path().join("target/gen.rs")),
            Relevance::Irrelevant
        ));
    }

    #[test]
    fn an_unrelated_file_never_triggers() {
        let (dir, mut set) = project_on_disk();
        assert!(matches!(
            set.classify(&dir.path().join("README.md")),
            Relevance::Irrelevant
        ));
    }

    /// Matching is by pattern, not by a frozen file list: a file created
    /// after the set was built still counts.
    #[test]
    fn a_file_created_after_startup_classifies_as_input() {
        let (dir, mut set) = project_on_disk();
        std::fs::write(dir.path().join("src/new.rs"), "fn new() {}").unwrap();
        assert!(matches!(
            set.classify(&dir.path().join("src/new.rs")),
            Relevance::Input
        ));
    }

    /// Deletion is a change like any other; the deleted path no longer
    /// exists, so classification must not require it to.
    #[test]
    fn a_deleted_watched_file_classifies_as_input() {
        let (dir, mut set) = project_on_disk();
        let file = dir.path().join("src/lib.rs");
        set.classify(&file); // seed: known while it existed
        std::fs::remove_file(&file).unwrap();
        assert!(matches!(set.classify(&file), Relevance::Input));
    }

    /// `.alba/` and `.git/` are always excluded, before any glob or
    /// gitignore reasoning — the cache writing its own state must never
    /// wake the session that owns it.
    #[test]
    fn alba_and_git_directories_are_excluded_outright() {
        let (dir, mut set) = project_on_disk();
        for special in [".alba/cache/build.json", ".git/index"] {
            assert!(matches!(
                set.classify(&dir.path().join(special)),
                Relevance::Irrelevant
            ));
        }
    }

    #[test]
    fn file_count_counts_the_resolved_inputs() {
        let (_dir, set) = project_on_disk();
        assert_eq!(set.file_count(), 1); // src/lib.rs; target/gen.rs is ignored
    }

    /// Only the target's subgraph is watched: `free`'s absence of inputs
    /// must not blank the set, and an unknown target is a `CoreError`.
    #[test]
    fn an_unknown_target_is_a_core_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Beamfile"), "beam a { run \"echo a\" }\n").unwrap();
        let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
        assert!(WatchSet::new(&project, &BeamId("missing".to_string()), &sources).is_err());
    }
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p alba-engine watch`
Expected: compile errors, module does not exist yet.

- [ ] **Step 3: Implement `WatchSet`**

`crates/alba-engine/src/watch/set.rs`:

```rust
//! Which changed paths concern a watch session: the loaded Beamfiles,
//! and the files the target subgraph's `inputs` patterns resolve to.
//!
//! Git-awareness is not reimplemented here: whether a path is a real
//! input is answered by re-running [`alba_core::expand_globs`], the same
//! function the cache fingerprints with, so the watch can never trigger
//! on something the cache would not see (or miss something it would).
//! The pattern match against the raw path is only a cheap pre-filter
//! that keeps the expensive re-expansion off the hot path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use alba_core::{BeamId, CoreError, Project, SourceMap, execution_subgraph, expand_globs};

/// What a changed path means to the session.
pub(crate) enum Relevance {
    /// A loaded Beamfile: reload the project before the next run.
    Beamfile,
    /// A file a subgraph beam's `inputs` resolve to: re-run.
    Input,
    /// Not the session's business: keep waiting.
    Irrelevant,
}

/// The watched universe of one loaded project, rebuilt on every reload.
pub(crate) struct WatchSet {
    beamfiles: Vec<PathBuf>,
    groups: Vec<InputGroup>,
}

/// One beam directory's worth of `inputs`: patterns are relative to the
/// defining Beamfile's directory (`Beam::dir`), so beams from different
/// imports resolve against different bases and cannot be pooled.
struct InputGroup {
    base: PathBuf,
    patterns: Vec<String>,
    set: globset::GlobSet,
    /// The last expansion's absolute paths, normalized. Membership means
    /// "known input"; a pattern match outside it forces a re-expansion,
    /// which is how created files are noticed and git-ignored ones are
    /// rejected.
    files: HashSet<PathBuf>,
}

impl WatchSet {
    pub(crate) fn new(
        project: &Project,
        target: &BeamId,
        sources: &SourceMap,
    ) -> Result<Self, CoreError> {
        let subgraph = execution_subgraph(project, target)?;
        let mut groups: Vec<InputGroup> = Vec::new();
        for beam in project.beams.iter().filter(|beam| subgraph.contains(&beam.id)) {
            if beam.inputs.is_empty() {
                continue;
            }
            let base = normalize(&beam.dir);
            match groups.iter_mut().find(|group| group.base == base) {
                Some(group) => {
                    for pattern in &beam.inputs {
                        if !group.patterns.contains(pattern) {
                            group.patterns.push(pattern.clone());
                        }
                    }
                }
                None => groups.push(InputGroup {
                    base,
                    patterns: beam.inputs.clone(),
                    set: globset::GlobSet::empty(),
                    files: HashSet::new(),
                }),
            }
        }
        for group in &mut groups {
            group.compile();
            group.expand();
        }
        Ok(Self {
            beamfiles: sources.paths().map(normalize).collect(),
            groups,
        })
    }

    pub(crate) fn file_count(&self) -> usize {
        self.groups.iter().map(|group| group.files.len()).sum()
    }

    pub(crate) fn classify(&mut self, path: &Path) -> Relevance {
        let path = normalize(path);
        if path
            .components()
            .any(|c| c.as_os_str() == ".git" || c.as_os_str() == ".alba")
        {
            return Relevance::Irrelevant;
        }
        if self.beamfiles.contains(&path) {
            return Relevance::Beamfile;
        }
        for group in &mut self.groups {
            if group.concerns(&path) {
                return Relevance::Input;
            }
        }
        Relevance::Irrelevant
    }
}

impl InputGroup {
    fn compile(&mut self) {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.patterns {
            // A pattern that does not compile was already reported by
            // `alba check`'s load-time validation; skipping it here
            // mirrors `expand_globs`' own degradation.
            if let Ok(glob) = globset::Glob::new(pattern) {
                builder.add(glob);
            }
        }
        self.set = builder.build().unwrap_or_else(|_| globset::GlobSet::empty());
    }

    fn expand(&mut self) {
        self.files = expand_globs(&self.base, &self.patterns)
            .into_iter()
            .map(|(_, absolute)| normalize(&absolute))
            .collect();
    }

    /// Whether `path` (already normalized) is one of this group's inputs.
    ///
    /// Known files answer from the snapshot — the common case, including
    /// deletions, whose paths stay in the snapshot until re-expansion. A
    /// pattern match outside the snapshot re-expands once: a created file
    /// lands in the fresh snapshot and is relevant; a git-ignored one
    /// never appears however often it changes, at the cost of one walk
    /// per debounced batch that names it — acceptable, because the
    /// typical ignored artifact (`target/`, `dist/`) fails the pattern
    /// match outright and never reaches the walk.
    fn concerns(&mut self, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(&self.base) else {
            return false;
        };
        let unified = relative.to_string_lossy().replace('\\', "/");
        if !self.set.is_match(&unified) {
            return false;
        }
        if self.files.contains(path) {
            return true;
        }
        self.expand();
        self.files.contains(path)
    }
}

/// A path in the same form the snapshots store: canonical when the path
/// exists, otherwise its existing parent's canonical form plus the final
/// component — a deleted file must still compare equal to the snapshot
/// entry recorded while it existed (macOS notably reports `/private/tmp`
/// for what a test created as `/tmp`).
fn normalize(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}
```

`crates/alba-engine/src/watch/mod.rs` for now:

```rust
//! Watch mode: re-run the target when its declared `inputs` change.
//! `set` decides which changed paths matter; the session loop arrives
//! with the rest of the module.

mod set;

pub(crate) use set::{Relevance, WatchSet};
```

Add `mod watch;` to `crates/alba-engine/src/lib.rs`, and `globset = { workspace = true }` (plus dev-dependency `tempfile` if missing) to `crates/alba-engine/Cargo.toml`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p alba-engine watch`
Expected: PASS. Note: the deleted-file test relies on the snapshot keeping the entry; if it flakes, the seed `classify` call is missing.

- [ ] **Step 5: Lints, then commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-engine
git commit -m "✨ feat(engine): classify file changes against the watched set"
```

---

### Task 4: The session loop

The core deliverable: `watch()` runs, waits, and restarts, driven by an injected `Watcher`. In this task a Beamfile change triggers a plain re-run; the actual reload is Task 5, so the loop's shape is reviewable on its own.

**Files:**
- Modify: `crates/alba-engine/src/watch/mod.rs`
- Modify: `crates/alba-engine/src/lib.rs` (exports)
- Modify: `crates/alba-engine/Cargo.toml` (add `async-trait = { workspace = true }`)
- Test: `crates/alba-engine/tests/watch.rs`

**Interfaces:**
- Consumes: `run`, `RunOptions`, `Executors`, `RunEvent`, `EngineError` (existing engine API); `WatchSet`/`Relevance` (Task 3); `RunEvent::Watch*` (Task 2).
- Produces (all `pub use`d from `alba_engine`):

```rust
pub enum WatchBatch { Paths(Vec<PathBuf>), Rescan }

#[async_trait::async_trait]
pub trait Watcher: Send {
    /// The next debounced batch; `None` when the watcher died for good.
    async fn next_batch(&mut self) -> Option<WatchBatch>;
}

pub enum WatchExit { Interrupted, WatcherClosed }

pub enum SessionError { Load(alba_core::LoadError), Run(EngineError) }

pub async fn watch(
    beamfile: &Path,
    project: Project,
    sources: SourceMap,
    target: BeamId,
    options: RunOptions,
    executors: Executors,
    events: UnboundedSender<RunEvent>,
    cancel: CancellationToken,
    watcher: Box<dyn Watcher>,
    on_error: &mut (dyn FnMut(&SessionError) + Send),
) -> WatchExit
```

`Project` and `SourceMap` are taken owned (both are `Clone`; the CLI clones what `main.rs` loaded). `beamfile` is the root path, used for reload (Task 5) and for making triggered paths root-relative.

- [ ] **Step 1: Write the failing tests**

`crates/alba-engine/tests/watch.rs`. The harness mirrors `tests/scheduler.rs`' habits (drained events, wide margins) but runs the session in a spawned task and scripts the watcher through a channel:

```rust
//! Behaviour tests for the watch session loop, driven through
//! [`alba_engine::watch`] with a scripted watcher and the
//! `FakeExecutor` — no real file watcher, no real process, so restart
//! and trigger semantics are asserted deterministically. Real files and
//! directories *are* used: the watched set resolves `inputs` on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alba_core::{BeamId, load_project};
use alba_engine::{
    Executors, RunEvent, RunOptions, SessionError, WatchBatch, WatchExit, Watcher, watch,
};
use alba_executors::{FakeBehavior, FakeExecutor};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

struct ScriptedWatcher {
    batches: UnboundedReceiver<WatchBatch>,
}

#[async_trait::async_trait]
impl Watcher for ScriptedWatcher {
    async fn next_batch(&mut self) -> Option<WatchBatch> {
        self.batches.recv().await
    }
}

/// A live watch session over a real temporary project, plus the handles
/// that drive and observe it.
struct Session {
    dir: tempfile::TempDir,
    executor: Arc<FakeExecutor>,
    batches: UnboundedSender<WatchBatch>,
    events: UnboundedReceiver<RunEvent>,
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<WatchExit>,
}

impl Session {
    /// The next event, or a panic after five seconds — a hung loop must
    /// fail the test, not the CI job's timeout.
    async fn event(&mut self) -> RunEvent {
        tokio::time::timeout(Duration::from_secs(5), self.events.recv())
            .await
            .expect("no event within 5s")
            .expect("event channel closed unexpectedly")
    }

    /// Drains until an event satisfies `matches`, returning it.
    async fn event_matching(&mut self, matches: impl Fn(&RunEvent) -> bool) -> RunEvent {
        loop {
            let event = self.event().await;
            if matches(&event) {
                return event;
            }
        }
    }

    fn touch(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.dir.path().join(relative);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn send(&self, paths: Vec<PathBuf>) {
        self.batches.send(WatchBatch::Paths(paths)).unwrap();
    }

    async fn finish(self) -> WatchExit {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("the session must end after cancellation")
            .expect("the session task must not panic")
    }
}

fn start(beamfile: &str, target: &str, executor: FakeExecutor, force: bool) -> Session {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.path().join("Beamfile"), beamfile).unwrap();

    let beamfile_path = dir.path().join("Beamfile");
    let (project, sources) = load_project(&beamfile_path).unwrap();
    let executor = Arc::new(executor);
    let (batches_tx, batches_rx) = unbounded_channel();
    let (events_tx, events_rx) = unbounded_channel();
    let cancel = CancellationToken::new();

    let options = RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: Some(alba_engine::CacheOptions {
            dir: dir.path().join(".alba").join("cache"),
            force,
        }),
    };
    let handle = tokio::spawn({
        let executor = Arc::clone(&executor);
        let cancel = cancel.clone();
        let target = BeamId(target.to_string());
        async move {
            watch(
                &beamfile_path,
                project,
                sources,
                target,
                options,
                Executors::uniform(executor),
                events_tx,
                cancel,
                Box::new(ScriptedWatcher { batches: batches_rx }),
                &mut |_: &SessionError| {},
            )
            .await
        }
    });

    Session {
        dir,
        executor,
        batches: batches_tx,
        events: events_rx,
        cancel,
        handle,
    }
}

const TWO_BEAMS: &str = "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n\
     beam docs { inputs [\"docs/**\"] needs [build] run \"echo docs\" }\n";

fn is_waiting(event: &RunEvent) -> bool {
    matches!(event, RunEvent::WatchWaiting { .. })
}

fn is_triggered(event: &RunEvent) -> bool {
    matches!(event, RunEvent::WatchTriggered { .. })
}

fn is_run_finished(event: &RunEvent) -> bool {
    matches!(event, RunEvent::RunFinished { .. })
}

/// The session's first act is a plain run, and only then does it wait.
#[tokio::test]
async fn runs_once_then_waits() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);

    session.event_matching(is_run_finished).await;
    session.event_matching(is_waiting).await;
    assert_eq!(session.executor.calls().len(), 2); // build + docs

    session.finish().await;
}

/// A relevant change triggers a new run; the untouched beam comes back
/// from the cache, so only the affected one executes again.
#[tokio::test]
async fn a_relevant_change_reruns_only_the_affected_beams() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed]);

    let triggered = session.event_matching(is_triggered).await;
    let RunEvent::WatchTriggered { paths } = triggered else { unreachable!() };
    assert_eq!(paths, vec!["src/main.rs".to_string()]);

    session.event_matching(is_run_finished).await;
    session.event_matching(is_waiting).await;
    // First run: build + docs. Second: build only — docs' inputs did not
    // change and its need succeeded from a fingerprint-identical state...
    // except build DID rerun with changed inputs, so docs reruns too.
    // What must hold: build ran twice; nothing ran a third time.
    let builds = session
        .executor
        .calls()
        .iter()
        .filter(|call| call.command.contains("compile"))
        .count();
    assert_eq!(builds, 2);

    session.finish().await;
}

/// Irrelevant paths do not wake the session: after an ignored batch, the
/// next relevant one is still the *first* trigger.
#[tokio::test]
async fn an_irrelevant_change_keeps_waiting() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let unrelated = session.touch("notes.txt", "not an input");
    session.send(vec![unrelated]);
    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed]);

    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert_eq!(paths, vec!["src/main.rs".to_string()]);

    session.finish().await;
}

/// A change landing mid-run cancels the run; the cancelled summary is
/// followed by the trigger and a fresh run. Current-thread runtime so
/// the scripted delay reliably keeps the run in flight when the batch
/// arrives.
#[tokio::test]
async fn a_mid_run_change_cancels_and_restarts() {
    let executor = FakeExecutor::new().on(
        "compile",
        FakeBehavior {
            exit_code: 0,
            delay: Duration::from_secs(30),
            output_lines: Vec::new(),
        },
    );
    let mut session = start(TWO_BEAMS, "docs", executor, false);

    session
        .event_matching(|event| matches!(event, RunEvent::BeamStarted { .. }))
        .await;
    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    session.send(vec![changed]);

    // The interrupted run ends (its long beam cancelled), the trigger is
    // announced after its summary, and a new run starts — well before the
    // scripted 30s delay could have elapsed on its own.
    let started = Instant::now();
    let RunEvent::RunFinished { summary } = session.event_matching(is_run_finished).await else {
        unreachable!()
    };
    assert!(!summary.cancelled.is_empty());
    session.event_matching(is_triggered).await;
    session
        .event_matching(|event| matches!(event, RunEvent::BeamStarted { .. }))
        .await;
    assert!(started.elapsed() < Duration::from_secs(10));

    session.finish().await;
}

/// Cancelling the session token ends the loop with `Interrupted`.
#[tokio::test]
async fn cancellation_ends_the_session() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;
    assert!(matches!(session.finish().await, WatchExit::Interrupted));
}

/// A watcher that dies takes the session with it — silently watching
/// nothing would look exactly like a healthy idle session.
#[tokio::test]
async fn a_closed_watcher_ends_the_session() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    drop(std::mem::replace(&mut session.batches, unbounded_channel().0));

    let exit = tokio::time::timeout(Duration::from_secs(5), session.handle)
        .await
        .expect("the session must end when the watcher closes")
        .expect("the session task must not panic");
    assert!(matches!(exit, WatchExit::WatcherClosed));
}

/// A rescan (overflow) triggers a run with no named paths.
#[tokio::test]
async fn a_rescan_triggers_a_run_with_no_paths() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    session.batches.send(WatchBatch::Rescan).unwrap();

    let RunEvent::WatchTriggered { paths } = session.event_matching(is_triggered).await else {
        unreachable!()
    };
    assert!(paths.is_empty());

    session.finish().await;
}

/// `--force` empties the cache's read side for the *initial* run only: a
/// triggered run with unchanged inputs comes back cached.
#[tokio::test]
async fn force_applies_to_the_initial_run_only() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), true);
    session.event_matching(is_waiting).await;
    let first_calls = session.executor.calls().len();
    assert_eq!(first_calls, 2);

    // Rewrite an input with identical content: relevant (it changed on
    // disk), but fingerprint-identical — cached unless force leaked.
    let changed = session.touch("src/main.rs", "fn main() {}");
    session.send(vec![changed]);
    session.event_matching(is_triggered).await;
    let RunEvent::RunFinished { summary } = session.event_matching(is_run_finished).await else {
        unreachable!()
    };
    assert_eq!(summary.cached.len(), 2);
    assert_eq!(session.executor.calls().len(), first_calls);

    session.finish().await;
}

/// The latency guard: from batch delivery to the triggered event must be
/// imperceptible. The bound is wide (2s vs the spec's tens of ms) so a
/// loaded CI machine cannot flip it; a regression to seconds still fails.
#[tokio::test]
async fn trigger_latency_stays_imperceptible() {
    let mut session = start(TWO_BEAMS, "docs", FakeExecutor::new(), false);
    session.event_matching(is_waiting).await;

    let changed = session.touch("src/main.rs", "fn main() { changed(); }");
    let sent_at = Instant::now();
    session.send(vec![changed]);
    session.event_matching(is_triggered).await;
    assert!(sent_at.elapsed() < Duration::from_secs(2));

    session.finish().await;
}
```

Fix the second test's comment before committing: `docs` reruns because its `need`'s fingerprint changed — the assertion on `builds == 2` plus "nothing ran a third time" (`calls().len() == 4`) is the meaningful pair. Assert `session.executor.calls().len() == 4` there instead of the loose comment.

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p alba-engine --test watch`
Expected: compile errors — `watch`, `Watcher`, `WatchBatch`, `WatchExit`, `SessionError` do not exist.

- [ ] **Step 3: Implement the loop**

`crates/alba-engine/src/watch/mod.rs`:

```rust
//! Watch mode: run the target, then re-run it whenever the files its
//! subgraph declares as `inputs` change.
//!
//! ## Shape
//!
//! [`watch`] owns the session: build the [`WatchSet`], run the target
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
//! A mid-session failure (an unschedulable run after a reload, a broken
//! Beamfile) must be *rendered* — spans, carets, suggestions — and
//! rendering lives in the CLI, above this crate. Embedding these errors
//! in [`crate::RunEvent`] would force `Clone` and a wire format on types
//! that exist to be pretty-printed once; a callback keeps the event
//! channel's contract clean and the session alive after reporting.

mod notify;
mod set;

use std::path::{Path, PathBuf};

use alba_core::{BeamId, Project, SourceMap};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::scheduler::{Executors, RunOptions, run};
use crate::{EngineError, RunEvent};
use set::{Relevance, WatchSet};

pub use notify::NotifyWatcher;

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
/// file system; `NotifyWatcher` is the real implementation.
#[async_trait::async_trait]
pub trait Watcher: Send {
    /// The next batch; `None` when the watcher died for good.
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
/// The session continues after every one of these.
pub enum SessionError {
    /// A Beamfile stopped loading (reload path). The session keeps
    /// watching and retries when a Beamfile changes again.
    Load(alba_core::LoadError),
    /// A run could not be carried out (unknown target after a rename,
    /// an unschedulable beam after an edit). Same posture: report, keep
    /// watching — the fix is one Beamfile save away.
    Run(EngineError),
}

/// What a batch amounted to once classified: whether a Beamfile was
/// touched, and the relevant paths for display.
struct Trigger {
    beamfile: bool,
    paths: Vec<PathBuf>,
}

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
    let mut project = project;
    let mut sources = sources;
    // `--force` empties the cache's read side for the initial run only:
    // applying it to every triggered run would re-run the whole subgraph
    // on every keystroke, which is exactly what the cache is here to
    // prevent.
    let mut force_next = options.cache.as_ref().is_some_and(|cache| cache.force);

    loop {
        let mut set = match WatchSet::new(&project, &target, &sources) {
            Ok(set) => set,
            Err(error) => {
                on_error(&SessionError::Run(EngineError::Core(error)));
                match reload_when_beamfile_changes(beamfile, &mut watcher, &cancel, on_error).await
                {
                    Reloaded::Project(p, s) => (project, sources) = (p, s),
                    Reloaded::Exit(exit) => return exit,
                }
                continue;
            }
        };

        // ---- run phase -------------------------------------------------
        let run_cancel = cancel.child_token();
        let mut run_options = options.clone();
        if let Some(cache) = run_options.cache.as_mut() {
            cache.force = force_next;
        }
        force_next = false;

        let mut pending: Option<Trigger> = None;
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
                            if let Some(trigger) = relevant(&mut set, batch) {
                                merge(&mut pending, trigger);
                                run_cancel.cancel();
                            }
                        }
                        None => {
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
        let trigger = match pending.take() {
            Some(trigger) => trigger,
            None => {
                let _ = events.send(RunEvent::WatchWaiting {
                    files: set.file_count(),
                });
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => return WatchExit::Interrupted,
                        batch = watcher.next_batch() => match batch {
                            Some(batch) => {
                                if let Some(trigger) = relevant(&mut set, batch) {
                                    break trigger;
                                }
                            }
                            None => return WatchExit::WatcherClosed,
                        },
                    }
                }
            }
        };
        let _ = events.send(RunEvent::WatchTriggered {
            paths: display_paths(&root, &trigger.paths),
        });
        if trigger.beamfile {
            // Task 5 replaces this comment with the reload; for now a
            // Beamfile change re-runs against the stale project.
        }
    }
}

/// Classifies a batch against the set; `None` when nothing in it
/// concerns the session.
fn relevant(set: &mut WatchSet, batch: WatchBatch) -> Option<Trigger> {
    match batch {
        WatchBatch::Rescan => Some(Trigger {
            beamfile: false,
            paths: Vec::new(),
        }),
        WatchBatch::Paths(paths) => {
            let mut trigger = Trigger {
                beamfile: false,
                paths: Vec::new(),
            };
            for path in paths {
                match set.classify(&path) {
                    Relevance::Beamfile => {
                        trigger.beamfile = true;
                        trigger.paths.push(path);
                    }
                    Relevance::Input => trigger.paths.push(path),
                    Relevance::Irrelevant => {}
                }
            }
            (trigger.beamfile || !trigger.paths.is_empty()).then_some(trigger)
        }
    }
}

fn merge(pending: &mut Option<Trigger>, fresh: Trigger) {
    match pending {
        Some(existing) => {
            existing.beamfile |= fresh.beamfile;
            for path in fresh.paths {
                if !existing.paths.contains(&path) {
                    existing.paths.push(path);
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

/// Placeholder until Task 5: never called yet, but typed now so the
/// broken-project branch above compiles.
enum Reloaded {
    Project(Project, SourceMap),
    Exit(WatchExit),
}

async fn reload_when_beamfile_changes(
    _beamfile: &Path,
    _watcher: &mut Box<dyn Watcher>,
    cancel: &CancellationToken,
    _on_error: &mut (dyn FnMut(&SessionError) + Send),
) -> Reloaded {
    cancel.cancelled().await;
    Reloaded::Exit(WatchExit::Interrupted)
}
```

Notes for the implementer:

- `mod notify;` will not compile until Task 6 creates the file. Create an empty `crates/alba-engine/src/watch/notify.rs` containing only `//! The notify-backed watcher arrives with the real integration.` and no items, or defer the `mod notify;`/`pub use notify::NotifyWatcher;` lines to Task 6 — either is fine; do not leave a broken build.
- `run` and the scheduler types are `pub(crate)`-reachable via `crate::scheduler` only if the module exposes them; they are already `pub use`d at the crate root, so `use crate::{Executors, RunOptions, run}` also works — match whichever the crate's internal style favors.
- In `crates/alba-engine/src/lib.rs`, export: `pub use watch::{NotifyWatcher, SessionError, WatchBatch, WatchExit, Watcher, watch};` (defer `NotifyWatcher` to Task 6 if `mod notify` was deferred).
- Add `async-trait = { workspace = true }` to `crates/alba-engine/Cargo.toml`, and `async-trait` plus `tempfile` to its dev-dependencies if the test file needs them (`ScriptedWatcher`'s impl uses `async_trait` from the test side too).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p alba-engine --test watch`
Expected: PASS, all nine tests.

- [ ] **Step 5: Run the whole workspace**

Run: `cargo test --workspace`
Expected: PASS — the scheduler and cache suites must be untouched by the new module.

- [ ] **Step 6: Lints, then commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-engine
git commit -m "✨ feat(engine): add the watch session loop"
```

---

### Task 5: Hot Beamfile reload

A Beamfile change reloads the project, recomputes subgraph and watched set, and re-runs. A Beamfile that no longer parses is reported and the session waits — executing nothing — until it parses again.

**Files:**
- Modify: `crates/alba-engine/src/watch/mod.rs`
- Test: `crates/alba-engine/tests/watch.rs`

**Interfaces:**
- Consumes: `alba_core::load_project`, `SessionError::Load`, the Task 4 loop.
- Produces: no signature change; the `trigger.beamfile` branch and `reload_when_beamfile_changes` become real.

- [ ] **Step 1: Write the failing tests**

Append to `crates/alba-engine/tests/watch.rs`. The error-observing tests need a shared error log; extend the harness with a variant of `start` that records session errors:

```rust
use std::sync::Mutex;

/// Session-error kinds observed, in order. The engine's error types are
/// not `Clone`, so the log keeps a tag per error rather than the error.
#[derive(Debug, PartialEq)]
enum ObservedError {
    Load,
    Run,
}

fn start_with_error_log(
    beamfile: &str,
    target: &str,
    executor: FakeExecutor,
) -> (Session, Arc<Mutex<Vec<ObservedError>>>) {
    // Identical to `start` except the closure passed as `on_error`:
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    // ... inside the spawned future, in place of `&mut |_| {}`:
    //   &mut move |error: &SessionError| {
    //       sink.lock().unwrap().push(match error {
    //           SessionError::Load(_) => ObservedError::Load,
    //           SessionError::Run(_) => ObservedError::Run,
    //       });
    //   },
    // (refactor `start` to take the closure as a parameter rather than
    // duplicating the body — `start` passes a no-op, this passes the log.)
    ...
}

/// Editing the Beamfile mid-session is picked up without a restart: the
/// next run executes the *new* command.
#[tokio::test]
async fn a_beamfile_change_reloads_and_reruns() {
    let mut session = start(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-v1\" }\n",
        "build",
        FakeExecutor::new(),
        false,
    );
    session.event_matching(is_waiting).await;

    let beamfile = session.touch(
        "Beamfile",
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-v2\" }\n",
    );
    session.send(vec![beamfile]);

    session.event_matching(is_triggered).await;
    session.event_matching(is_run_finished).await;
    let commands: Vec<String> = session
        .executor
        .calls()
        .iter()
        .map(|call| call.command.clone())
        .collect();
    assert!(commands.iter().any(|c| c.contains("compile-v1")));
    assert!(commands.iter().any(|c| c.contains("compile-v2")));

    session.finish().await;
}

/// A Beamfile that stops parsing is reported, nothing executes, and the
/// session resumes as soon as it parses again.
#[tokio::test]
async fn a_broken_beamfile_reports_waits_and_recovers() {
    let (mut session, errors) = start_with_error_log(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
        "build",
        FakeExecutor::new(),
    );
    session.event_matching(is_waiting).await;
    let calls_before = session.executor.calls().len();

    let beamfile = session.touch("Beamfile", "beam build { this does not parse");
    session.send(vec![beamfile.clone()]);
    session.event_matching(is_triggered).await;

    // The load failure is reported; no run happens on a broken project.
    // Give the loop a moment to (wrongly) start one before asserting.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*errors.lock().unwrap(), vec![ObservedError::Load]);
    assert_eq!(session.executor.calls().len(), calls_before);

    // The fix arrives: reload succeeds and the session runs again.
    session.touch(
        "Beamfile",
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-fixed\" }\n",
    );
    session.send(vec![beamfile]);
    session.event_matching(is_run_finished).await;
    assert!(
        session
            .executor
            .calls()
            .iter()
            .any(|call| call.command.contains("compile-fixed"))
    );

    session.finish().await;
}

/// A target that vanishes on reload (renamed beam) is a run error, not a
/// crash: reported, and the session waits for the next Beamfile change.
#[tokio::test]
async fn a_renamed_target_reports_and_recovers_on_the_next_edit() {
    let (mut session, errors) = start_with_error_log(
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
        "build",
        FakeExecutor::new(),
    );
    session.event_matching(is_waiting).await;

    let beamfile = session.touch(
        "Beamfile",
        "beam renamed { inputs [\"src/**/*.rs\"] run \"echo compile\" }\n",
    );
    session.send(vec![beamfile.clone()]);
    session.event_matching(is_triggered).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(errors.lock().unwrap().contains(&ObservedError::Run));

    session.touch(
        "Beamfile",
        "beam build { inputs [\"src/**/*.rs\"] run \"echo compile-back\" }\n",
    );
    session.send(vec![beamfile]);
    session.event_matching(is_run_finished).await;

    session.finish().await;
}
```

Write `start_with_error_log` for real (the sketch above marks the two edits to make); refactor `start` so both share one body.

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p alba-engine --test watch`
Expected: the three new tests FAIL (`compile-v2` never runs; the load error is never reported).

- [ ] **Step 3: Implement the reload**

In `watch/mod.rs`, replace the `trigger.beamfile` placeholder branch:

```rust
        if trigger.beamfile {
            match alba_core::load_project(beamfile) {
                Ok((fresh_project, fresh_sources)) => {
                    (project, sources) = (fresh_project, fresh_sources);
                }
                Err(error) => {
                    on_error(&SessionError::Load(error));
                    match reload_when_beamfile_changes(beamfile, &mut watcher, &cancel, on_error)
                        .await
                    {
                        Reloaded::Project(p, s) => (project, sources) = (p, s),
                        Reloaded::Exit(exit) => return exit,
                    }
                }
            }
        }
```

And make `reload_when_beamfile_changes` real — the broken-project idle state. While broken, only Beamfile changes matter; classification against the stale `WatchSet` would work for known Beamfiles, but the simplest honest rule is: any batch retries the reload (a reload attempt is one file read plus a parse — cheap — and a batch only arrives after a real debounced change):

```rust
/// The broken-project idle state: the last load (or plan) failed and was
/// reported; nothing may execute. Every subsequent batch retries the
/// reload — a batch means something changed, and a retry is one file
/// read plus a parse. Repeated failures are reported each time: the user
/// just saved the file and is looking at the terminal for an answer.
async fn reload_when_beamfile_changes(
    beamfile: &Path,
    watcher: &mut Box<dyn Watcher>,
    cancel: &CancellationToken,
    on_error: &mut (dyn FnMut(&SessionError) + Send),
) -> Reloaded {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return Reloaded::Exit(WatchExit::Interrupted),
            batch = watcher.next_batch() => match batch {
                Some(_) => match alba_core::load_project(beamfile) {
                    Ok((project, sources)) => return Reloaded::Project(project, sources),
                    Err(error) => on_error(&SessionError::Load(error)),
                },
                None => return Reloaded::Exit(WatchExit::WatcherClosed),
            },
        }
    }
}
```

One consequence to preserve: after `reload_when_beamfile_changes` returns a fresh project, the outer loop `continue`s — in the *broken-during-trigger* path above, fall through to the top of the loop (the next iteration rebuilds the `WatchSet` and runs). Make sure the `WatchSet::new` failure branch from Task 4 and this branch converge on the same "fresh project, next iteration runs it" behaviour. Note the broken-state test's expectation: the run only happens after the *fix* batch, and it did — `reload_when_beamfile_changes` returned, the loop iterated, the run phase executed immediately (no extra `WatchTriggered` is emitted for the recovery run; the earlier `WatchTriggered` already announced this cycle).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p alba-engine --test watch`
Expected: PASS, all twelve tests.

- [ ] **Step 5: Lints, then commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-engine
git commit -m "✨ feat(engine): reload the Beamfile during a watch session"
```

---

### Task 6: `NotifyWatcher` — the real file watcher

The `notify`-backed `Watcher`: recursive watches on the given roots, 200 ms debounce, errors surfaced as `Rescan`.

**Files:**
- Create: `crates/alba-engine/src/watch/notify.rs`
- Modify: `crates/alba-engine/src/watch/mod.rs` (`mod notify; pub use notify::NotifyWatcher;` if deferred in Task 4)
- Modify: `crates/alba-engine/src/lib.rs` (export `NotifyWatcher` if deferred)
- Modify: `Cargo.toml` (workspace) and `crates/alba-engine/Cargo.toml`

**Interfaces:**
- Produces:

```rust
pub struct NotifyWatcher { /* receiver + kept-alive debouncer */ }
impl NotifyWatcher {
    /// The built-in debounce window. A constant, not a flag.
    pub const DEBOUNCE: Duration = Duration::from_millis(200);
    /// Starts watching every root recursively. An error here is fatal to
    /// the session before it starts — the CLI turns it into exit code 2.
    pub fn new(roots: &[PathBuf]) -> Result<Self, notify::Error>;
}
```

- [ ] **Step 1: Add the dependencies**

```bash
cargo add notify notify-debouncer-full --package alba-engine
```

Then move the two version specs up into `[workspace.dependencies]` in the root `Cargo.toml` (matching how every other dependency is declared) and reference them with `{ workspace = true }` in `crates/alba-engine/Cargo.toml`. Note the exact versions cargo picked; the debouncer's API has shifted across releases, so read the resolved version's docs (`cargo doc -p notify-debouncer-full --open` or docs.rs) before Step 3 and adapt the constructor call if its signature differs from the sketch.

- [ ] **Step 2: Write the failing test**

Append to `crates/alba-engine/tests/watch.rs` (integration over unit: the value under test is the real notify wiring):

```rust
/// The one test that exercises the real file watcher: a write on disk
/// must come through as a batch naming the file. Everything else in this
/// suite scripts batches; this proves the scripting matches reality on
/// each platform.
#[tokio::test]
async fn notify_watcher_reports_a_real_write() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("watched.txt"), "before").unwrap();

    let mut watcher =
        alba_engine::NotifyWatcher::new(&[dir.path().to_path_buf()]).expect("watcher must start");

    // Give the OS watcher a moment to arm before the write, then write.
    tokio::time::sleep(Duration::from_millis(250)).await;
    std::fs::write(dir.path().join("watched.txt"), "after").unwrap();

    let batch = tokio::time::timeout(Duration::from_secs(10), watcher.next_batch())
        .await
        .expect("a batch must arrive within 10s")
        .expect("the watcher must not close");
    match batch {
        WatchBatch::Paths(paths) => assert!(
            paths.iter().any(|p| p.ends_with("watched.txt")),
            "batch must name the written file, got {paths:?}"
        ),
        WatchBatch::Rescan => {} // an overflow still reports a change; acceptable
    }
}
```

- [ ] **Step 3: Run it to make sure it fails, then implement**

Run: `cargo test -p alba-engine --test watch notify_watcher`
Expected: compile error, `NotifyWatcher` not found.

`crates/alba-engine/src/watch/notify.rs`:

```rust
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
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{WatchBatch, Watcher};

pub struct NotifyWatcher {
    batches: UnboundedReceiver<WatchBatch>,
    // Kept for its Drop; see the module doc comment. The concrete type
    // is what `new_debouncer` returns for the platform's recommended
    // watcher — spell it out or box it, whichever the resolved
    // notify-debouncer-full version makes shorter.
    _debouncer: notify_debouncer_full::Debouncer<
        notify::RecommendedWatcher,
        notify_debouncer_full::RecommendedCache,
    >,
}

impl NotifyWatcher {
    /// The built-in debounce window: long enough to coalesce an editor's
    /// save burst or a `git checkout`, short enough to feel immediate.
    pub const DEBOUNCE: Duration = Duration::from_millis(200);

    pub fn new(roots: &[PathBuf]) -> Result<Self, notify::Error> {
        let (sender, batches) = unbounded_channel();
        let mut debouncer = notify_debouncer_full::new_debouncer(
            Self::DEBOUNCE,
            None,
            move |result: notify_debouncer_full::DebounceEventResult| {
                let batch = match result {
                    Ok(events) => WatchBatch::Paths(
                        events.into_iter().flat_map(|event| event.paths.clone()).collect(),
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
```

If the resolved `notify-debouncer-full` version exposes `debouncer.watcher().watch(...)` instead of `debouncer.watch(...)`, or names the cache type differently, follow the resolved version — the contract to preserve is: recursive watch on every root, `DEBOUNCE` window, `Rescan` on error, channel closed only when the debouncer dies.

- [ ] **Step 4: Run the test on the real file system**

Run: `cargo test -p alba-engine --test watch notify_watcher`
Expected: PASS.

- [ ] **Step 5: Full workspace, lints, commit**

```bash
cargo test --workspace
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add Cargo.toml Cargo.lock crates/alba-engine
git commit -m "✨ feat(engine): drive the watch loop from notify"
```

---

### Task 7: `alba run` without a beam runs the default

`alba run --watch` must work bare (spec: composes with the default beam). Today `beam` is a required positional; it becomes optional, resolving to the Beamfile's `default`, with a clear error when there is none.

**Files:**
- Modify: `crates/alba-cli/src/args.rs` (`beam: Option<String>`)
- Modify: `crates/alba-cli/src/main.rs` (resolution)
- Test: `crates/alba-cli/tests/cli_run.rs`

**Interfaces:**
- Consumes: `Project::default` (`Option<Spanned<BeamId>>`), `EXIT_ALBA_ERROR`.
- Produces: `Command::Run { beam: Option<String>, .. }`; bare `alba run` behaves like bare `alba` with flags available.

- [ ] **Step 1: Write the failing tests**

Append to `crates/alba-cli/tests/cli_run.rs`, following the file's `alba()`/`project()` helpers:

```rust
/// `alba run` with no beam named runs the declared `default`, exactly
/// like bare `alba` — but with the run flags available.
#[test]
fn run_without_a_beam_runs_the_default() {
    let dir = project(
        "default hello\n\
         beam hello { run \"echo salut\" }\n",
    );

    alba()
        .current_dir(&dir)
        .args(["run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("salut"));
}

/// Without a `default`, `alba run` cannot guess; it says so and exits 2.
#[test]
fn run_without_a_beam_and_no_default_is_an_error() {
    let dir = project("beam hello { run \"echo salut\" }\n");

    alba()
        .current_dir(&dir)
        .args(["run"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("default"));
}
```

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p alba-cli --test cli_run run_without`
Expected: FAIL — clap rejects the missing required argument (exit 2 with a usage error, but no `default` mention; the first test fails outright).

- [ ] **Step 3: Implement**

In `args.rs`, `Command::Run`'s `beam` field becomes:

```rust
        /// The beam to run. Defaults to the Beamfile's `default` beam.
        #[arg(value_name = "BEAM")]
        beam: Option<String>,
```

In `main.rs`, the `Some(Command::Run { .. })` arm resolves the target before calling `commands::run::run`:

```rust
        Some(Command::Run {
            beam,
            params,
            flags,
        }) => {
            let target = beam
                .map(alba_core::BeamId)
                .or_else(|| project.default.as_ref().map(|d| d.value.clone()));
            match target {
                Some(target) => {
                    commands::run::run(&project, &sources, &beamfile, &target, params, &flags)
                }
                None => {
                    LineSink::stderr().line(
                        "no beam named and this Beamfile declares no `default`; \
                         run `alba` to list the available beams",
                    );
                    EXIT_ALBA_ERROR
                }
            }
        }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p alba-cli --test cli_run`
Expected: PASS, existing tests included.

- [ ] **Step 5: Lints, then commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-cli
git commit -m "✨ feat(cli): run the default beam when no beam is named"
```

---### Task 8: `--watch` on the CLI, end to end

The flag, the watch execution path (watcher construction, no-inputs warning, error rendering, clear-screen, exit codes), and the e2e tests that drive a real long-running process through the JSON stream.

**Files:**
- Modify: `crates/alba-cli/src/args.rs` (the flag)
- Modify: `crates/alba-cli/src/main.rs` (`render_load_error` reachable from commands)
- Modify: `crates/alba-cli/src/commands/run.rs` (the watch path)
- Test: `crates/alba-cli/tests/cli_watch.rs`
- Modify: `crates/alba-cli/Cargo.toml` (dev-dependency `nix` if not already there for the existing interrupt tests)

**Interfaces:**
- Consumes: `alba_engine::{watch, NotifyWatcher, SessionError, WatchExit}` (Tasks 4-6), `SourceMap::paths()` (Task 1), renderer constructors with `clear_between_runs` (Task 2), optional beam (Task 7).
- Produces: `RunFlags::watch: bool`; `alba run --watch [beam] [args]`.

- [ ] **Step 1: Write the failing e2e tests**

`crates/alba-cli/tests/cli_watch.rs`:

```rust
//! End-to-end tests for `alba run --watch`: a real long-running process,
//! observed through `--log-format json` (the deterministic oracle) on a
//! reader thread. Commands are `echo`-only so macOS, Linux, and Windows
//! behave identically; the exit-code-on-SIGINT test is `#[cfg(unix)]`
//! like the existing interrupt tests.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

struct WatchProcess {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl WatchProcess {
    fn spawn(dir: &Path, args: &[&str]) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("alba"))
            .current_dir(dir)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// Reads lines until one contains `needle`, or panics after 60s with
    /// everything seen so far — a hung watch must fail with context.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    self.seen.push(line.clone());
                    if line.contains(needle) {
                        return line;
                    }
                }
                Err(_) => panic!(
                    "never saw {needle:?}; output so far:\n{}",
                    self.seen.join("\n")
                ),
            }
        }
    }
}

impl Drop for WatchProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn watch_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/input.txt"), "one").unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "default hello\n\
         beam hello { inputs [\"src/**\"] run \"echo greeting-ran\" }\n",
    )
    .unwrap();
    dir
}

/// The full cycle: initial run, waiting, a real file change, a
/// triggered second run.
#[test]
fn a_file_change_triggers_a_second_run() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(
        dir.path(),
        &["run", "--watch", "hello", "--log-format", "json"],
    );

    alba.wait_for(r#""event":"run_finished""#);
    alba.wait_for(r#""event":"watch_waiting""#);

    std::fs::write(dir.path().join("src/input.txt"), "two").unwrap();

    let triggered = alba.wait_for(r#""event":"watch_triggered""#);
    assert!(triggered.contains("src/input.txt"), "got: {triggered}");
    alba.wait_for(r#""event":"run_finished""#);
    alba.wait_for(r#""event":"watch_waiting""#);
}

/// `--watch` composes with the default beam: no beam named on the
/// command line.
#[test]
fn watch_runs_the_default_beam() {
    let dir = watch_project();
    let mut alba =
        WatchProcess::spawn(dir.path(), &["run", "--watch", "--log-format", "json"]);
    alba.wait_for(r#""beam":"hello""#);
}

/// Editing the Beamfile mid-session is a reload, not a stale re-run:
/// the next run carries the new command's output.
#[test]
fn a_beamfile_edit_reloads_the_project() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(
        dir.path(),
        &["run", "--watch", "hello", "--log-format", "json"],
    );
    alba.wait_for(r#""event":"watch_waiting""#);

    std::fs::write(
        dir.path().join("Beamfile"),
        "default hello\n\
         beam hello { inputs [\"src/**\"] run \"echo greeting-edited\" }\n",
    )
    .unwrap();

    alba.wait_for(r#""event":"watch_triggered""#);
    alba.wait_for("greeting-edited");
}

/// Ctrl-C ends the session with exit code 0: the runs already reported
/// themselves, and an orderly goodbye is not a failure.
#[cfg(unix)]
#[test]
fn sigint_ends_the_session_with_code_0() {
    let dir = watch_project();
    let mut alba = WatchProcess::spawn(
        dir.path(),
        &["run", "--watch", "hello", "--log-format", "json"],
    );
    alba.wait_for(r#""event":"watch_waiting""#);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(alba.child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();

    let status = alba.child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
}
```

Check `crates/alba-cli/Cargo.toml` for the `nix` dev-dependency (the existing `#[cfg(unix)]` interrupt tests in `cli_run.rs` very likely already pull it — read how they send their signal and reuse the same mechanism verbatim).

- [ ] **Step 2: Run them to make sure they fail**

Run: `cargo test -p alba-cli --test cli_watch`
Expected: FAIL — `--watch` is an unknown flag; every test panics in `wait_for`.

- [ ] **Step 3: Implement the flag and the watch path**

`args.rs`, in `RunFlags`:

```rust
    /// Keep running: re-run the beam whenever the files its subgraph
    /// declares as `inputs` change. Ctrl-C ends the session.
    #[arg(long)]
    pub watch: bool,
```

`main.rs`: make `render_load_error` callable from the commands module (`pub(crate) fn render_load_error(...)` — it already lives in `main.rs`, which is the crate root of the binary, so only the visibility changes).

`commands/run.rs`:

1. `run()` branches: `if flags.watch { runtime.block_on(watch_execute(project, sources, beamfile, target, params, flags)) } else { runtime.block_on(execute(...)) }`.
2. `watch_execute`, alongside `execute`:

```rust
/// The `--watch` counterpart of [`execute`]: same renderer, same
/// interrupt watcher, but the engine's session loop instead of a single
/// run, and watch-specific exit codes — 0 for an orderly Ctrl-C (the
/// runs already reported themselves), 2 only for startup failures.
async fn watch_execute(
    project: &Project,
    sources: &SourceMap,
    beamfile: &Path,
    target: &BeamId,
    params: Vec<String>,
    flags: &RunFlags,
) -> i32 {
    let mut err = LineSink::stderr();

    // The subgraph may declare no inputs at all; the session would then
    // only ever react to Beamfile edits. Legitimate (hot reload makes it
    // self-repairing) but surprising, so it is said out loud once.
    match alba_core::execution_subgraph(project, target) {
        Ok(subgraph) => {
            let no_inputs = project
                .beams
                .iter()
                .filter(|beam| subgraph.contains(&beam.id))
                .all(|beam| beam.inputs.is_empty());
            if no_inputs {
                err.line(&format!(
                    "warning: no beam in `{}`'s graph declares inputs; \
                     watching the Beamfile only",
                    target.0
                ));
            }
        }
        Err(error) => {
            err.line(crate::render_core_error(error, sources).trim_end());
            return EXIT_ALBA_ERROR;
        }
    }

    let watcher = match alba_engine::NotifyWatcher::new(&watch_roots(beamfile, sources)) {
        Ok(watcher) => watcher,
        Err(error) => {
            err.line(&format!("cannot start the file watcher: {error}"));
            return EXIT_ALBA_ERROR;
        }
    };

    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
        cache: Some(CacheOptions {
            dir: cache_dir(beamfile),
            force: flags.force,
        }),
    };
    let (events, incoming) = unbounded_channel();
    let cancel = CancellationToken::new();
    tokio::spawn(watch_interrupts(cancel.clone()));
    let consumer = tokio::spawn(consume(incoming, watch_renderer(flags), cancel.clone()));

    let exit = alba_engine::watch(
        beamfile,
        project.clone(),
        sources.clone(),
        target.clone(),
        options,
        Executors {
            embedded: Arc::new(EmbeddedShellExecutor),
            system: Arc::new(SystemShellExecutor),
        },
        events,
        cancel,
        Box::new(watcher),
        &mut |error| {
            let mut err = LineSink::stderr();
            match error {
                alba_engine::SessionError::Load(load) => {
                    err.line(crate::render_load_error_ref(load).trim_end())
                }
                alba_engine::SessionError::Run(run) => {
                    err.line(render_engine_error_ref(run, sources).trim_end())
                }
            }
        },
    )
    .await;

    let renderer_panicked = consumer.await.is_err();
    if renderer_panicked {
        LineSink::stderr().line("the output renderer panicked; this session's report is incomplete");
        return EXIT_ALBA_ERROR;
    }
    match exit {
        alba_engine::WatchExit::Interrupted => 0,
        alba_engine::WatchExit::WatcherClosed => {
            LineSink::stderr().line("the file watcher stopped; ending the session");
            EXIT_ALBA_ERROR
        }
    }
}

/// The directories `NotifyWatcher` puts under recursive watch: the
/// project root, plus the directory of any Beamfile loaded from outside
/// it (an import in a sibling tree). Roots are fixed for the session;
/// an import *added mid-session* that lives outside these roots emits
/// no events until the next `alba run --watch` — documented limitation.
fn watch_roots(beamfile: &Path, sources: &SourceMap) -> Vec<PathBuf> {
    let root = beamfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let root = std::path::absolute(&root).unwrap_or(root);
    let mut roots = vec![root.clone()];
    for path in sources.paths() {
        if let Some(parent) = path.parent() {
            let parent = std::path::absolute(parent).unwrap_or_else(|_| parent.to_path_buf());
            if !parent.starts_with(&root) && !roots.contains(&parent) {
                roots.push(parent);
            }
        }
    }
    roots
}

/// The watch session's renderer: same selection as [`renderer`], but the
/// text renderers clear the screen between runs when stdout is a
/// terminal — a clean page per run. Never with JSON (a machine is
/// parsing) and never off-TTY (a log is accumulating).
fn watch_renderer(flags: &RunFlags) -> Box<dyn Renderer> {
    let clear = flags.log_format == LogFormat::Text && std::io::stdout().is_terminal();
    match flags.log_format {
        LogFormat::Json => Box::new(JsonRenderer::new()),
        LogFormat::Text => match flags.output.unwrap_or_else(default_output) {
            OutputStyle::Interleaved => {
                Box::new(InterleavedRenderer::new(crate::color_enabled(), clear))
            }
            OutputStyle::Grouped => Box::new(GroupedRenderer::new(clear)),
        },
    }
}
```

Adaptation notes, to resolve while implementing (read the existing code first, follow its style):

- `render_load_error` in `main.rs` takes the error by value today (it is called with the owned error). The callback only has `&LoadError`; either add a by-reference variant (`render_load_error_ref`) or restructure the existing function to take `&LoadError` and update its one call site — prefer the latter (one function, not two). Same question for `render_engine_error`, which consumes `EngineError`; make it take `&EngineError` and adjust its call site in `execute`. The sketch's `_ref` names disappear in that case.
- `consume`'s `cancelling...` announcement fires on the session token in watch mode too — correct as is (a Ctrl-C mid-run should say it). No change needed.
- The interrupt watcher's second-Ctrl-C hard exit uses `EXIT_INTERRUPTED` (130); the *orderly* watch exit is 0 by the `WatchExit::Interrupted` arm above. Both are intended: the second Ctrl-C is an abort, not an orderly goodbye.
- `watch_interrupts` and `consume` are reused untouched.

- [ ] **Step 4: Run the e2e tests**

Run: `cargo test -p alba-cli --test cli_watch`
Expected: PASS. These tests compile the real binary; the first run is slow — that is normal.

- [ ] **Step 5: Full workspace, lints, commit**

```bash
cargo test --workspace
cargo fmt --check && cargo clippy --all-targets -- -D warnings
git add crates/alba-cli
git commit -m "✨ feat(cli): add alba run --watch"
```

---

### Task 9: Documentation and final verification

**Files:**
- Modify: `README.md`

**Interfaces:**
- Consumes: everything shipped above.
- Produces: user-facing documentation; a verified workspace.

- [ ] **Step 1: Document watch mode in the README**

- In the `### Flags` section, add `--watch` to the flag table/list, phrased like its neighbors: re-run the beam whenever the files its subgraph declares as `inputs` change.
- Add a `## Watch mode` section after `## Caching` covering, in the README's existing voice: what `alba run --watch [beam]` does; that triggers are the subgraph's `inputs` (the same declarations the cache fingerprints — one source of truth); cancel-and-restart on mid-run changes; hot Beamfile reload including the broken-Beamfile behaviour (diagnostics, nothing executes, resumes on the next successful parse); `--force` applying to the initial run only; the ~200 ms debounce; Ctrl-C ending the session with exit code 0; the clear-screen-on-TTY behaviour; the feedback-loop caveat (a beam writing to a git-tracked file matched by `inputs` will loop — keep outputs git-ignored); and the fixed-roots limitation (an import added mid-session outside the project root needs a session restart to be watched).
- Mention the two JSON events (`watch_waiting`, `watch_triggered`) wherever the README documents the JSON stream.
- Update the `### Exit codes` section: `alba run --watch` exits 0 on Ctrl-C, 2 on startup errors (including a watcher that cannot start).

- [ ] **Step 2: Dogfood on the repository itself**

The repository's `Beamfile` already declares real `inputs`. Sanity-run (a few seconds each, then Ctrl-C):

```bash
cargo run -- run --watch test    # touch a crates/**/*.rs file; observe the re-run
cargo run -- run --watch         # composes with `default check`
```

This is a manual observation step, not a scripted test; confirm the trigger, the cached beams, and the exit-0 Ctrl-C by eye.

- [ ] **Step 3: Full verification**

```bash
cargo test --workspace
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo run -- check
```

Expected: everything green.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "📝 docs: document watch mode"
```

---

## Self-review checklist (run after writing, before handoff)

- Spec coverage: trigger = subgraph `inputs` + Beamfiles (Tasks 3-4); `--watch` on `alba run` incl. default beam (Tasks 7-8); cancel-and-restart (Task 4); hot reload incl. broken state (Task 5); events on the shared channel + JSON (Task 2); debounce constant + notify (Task 6); `--force` first-run-only (Task 4); Ctrl-C exit 0 + startup exit 2 (Task 8); no-inputs warning (Task 8); clear-screen on TTY (Tasks 2, 8); `.alba`/`.git`/gitignore exclusions (Task 3); rescan-as-trigger (Tasks 4, 6); latency guard (Task 4); e2e on a real process (Task 8); README (Task 9).
- Interruption marker: implemented as event *ordering* (cancelled summary, then `WatchTriggered`) — see the spec-to-plan mapping section; renderers phrase the trigger line accordingly.
- Every task ends in a commit with the workspace green; no task depends on a later one.
