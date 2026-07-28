# Alba Cache / Incremental Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Skip-only caching for beams: fingerprint declared `inputs`, skip unchanged beams with a `cached` status, replay their stored logs, expand globs `.gitignore`-aware, and expose `--force` and `alba cache clean`.

**Architecture:** A `cache` module inside `alba-engine` (fingerprint, store, decision), consulted by the scheduler when a beam becomes ready. Glob expansion lives in `alba-core` (`.gitignore`-aware via the `ignore` crate). State persists under `.alba/cache/` at the project root; only plain successes write it. The event channel gains a `BeamCached` event, a `Cached` status, and a `replayed` marker on output lines.

**Tech Stack:** Rust (edition 2024), tokio, blake3 (hashing), ignore + globset (walking and matching), serde/serde_json (manifest), insta-free (plain asserts), assert_cmd + tempfile (end-to-end tests).

**Spec:** `.claude/superpowers/specs/2026-07-28-alba-cache-design.md`

## Global Constraints

- Workspace already exists; dependency rules stay `cli → engine → core → syntax`, and the engine keeps seeing executors only through the `Executor` trait. The cache never talks to executors.
- New dependencies are declared in the workspace `Cargo.toml` (`[workspace.dependencies]`) and referenced with `.workspace = true`: `blake3 = "1"`, `ignore = "0.4"`, `globset = "0.4"`.
- TDD for every task: failing test first, minimal implementation, green, then commit.
- Every commit must pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace`.
- Commit messages: gitmoji + Conventional Commits (e.g. `✨ feat(engine): ...`). Never add Claude attribution or reference this plan in commits.
- All file content, comments, and messages in English. Match the existing comment density and voice (doc comments explain *why*, not *what*).
- The cache must never fail a run: every cache-side error degrades to "run the beam" (a miss or non-cacheable), never to an `EngineError`.
- Exit codes are unchanged: 0 success, 1 beam failure, 2 Alba error. A cached beam counts toward exit code 0.

---

### Task 1: `.gitignore`-aware glob expansion and outputs check (`alba-core`)

**Files:**
- Modify: `Cargo.toml` (workspace: add `ignore`, `globset` to `[workspace.dependencies]`)
- Modify: `crates/alba-core/Cargo.toml` (add `ignore.workspace = true`, `globset.workspace = true`)
- Create: `crates/alba-core/src/files.rs`
- Modify: `crates/alba-core/src/lib.rs` (add `mod files;` and `pub use files::{expand_globs, outputs_satisfied};`)
- Test: `crates/alba-core/tests/files.rs`

**Interfaces:**
- Consumes: nothing from other tasks (leaf task).
- Produces:
  - `pub fn expand_globs(base: &Path, patterns: &[String]) -> Vec<(String, PathBuf)>` — files under `base` matching any pattern, `.gitignore`-aware, skipping `.git/` and `.alba/`. Each entry is `(relative path with '/' separators, absolute path)`, sorted by relative path. Invalid patterns yield an empty list (patterns were already validated at load time; this function never fails).
  - `pub fn outputs_satisfied(base: &Path, patterns: &[String]) -> bool` — raw disk check, NOT `.gitignore`-aware (outputs live in ignored directories like `target/`): a literal pattern must exist as a path, a pattern containing `*`, `?`, or `[` must match at least one file via the `glob` crate. An empty list is satisfied.

- [ ] **Step 1: Add the dependencies**

In the workspace `Cargo.toml` `[workspace.dependencies]` (alphabetical position):

```toml
blake3 = "1"
globset = "0.4"
ignore = "0.4"
```

(`blake3` is added now so the workspace section is touched once; Task 2 uses it.)

In `crates/alba-core/Cargo.toml` `[dependencies]`:

```toml
globset.workspace = true
ignore.workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/alba-core/tests/files.rs`:

```rust
//! Behaviour tests for `expand_globs` and `outputs_satisfied`: the
//! filesystem half of the cache's fingerprinting, exercised against real
//! temporary directories.

use std::path::Path;

use alba_core::{expand_globs, outputs_satisfied};

fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn relative_paths(dir: &Path, patterns: &[&str]) -> Vec<String> {
    let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
    expand_globs(dir, &patterns)
        .into_iter()
        .map(|(relative, _)| relative)
        .collect()
}

#[test]
fn matches_files_by_glob_and_returns_them_sorted() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/b.rs", "b");
    write(dir.path(), "src/a.rs", "a");
    write(dir.path(), "src/nested/c.rs", "c");
    write(dir.path(), "readme.md", "docs");

    assert_eq!(
        relative_paths(dir.path(), &["src/**/*.rs"]),
        vec!["src/a.rs", "src/b.rs", "src/nested/c.rs"]
    );
}

#[test]
fn a_literal_pattern_matches_exactly_one_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Cargo.toml", "[package]");
    write(dir.path(), "Cargo.lock", "lock");

    assert_eq!(relative_paths(dir.path(), &["Cargo.toml"]), vec!["Cargo.toml"]);
}

#[test]
fn respects_gitignore_even_outside_a_git_repository() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "generated/\n");
    write(dir.path(), "src/a.rs", "a");
    write(dir.path(), "generated/b.rs", "b");

    assert_eq!(relative_paths(dir.path(), &["**/*.rs"]), vec!["src/a.rs"]);
}

#[test]
fn never_walks_git_or_alba_directories() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".git/objects/x.rs", "not source");
    write(dir.path(), ".alba/cache/y.rs", "not source");
    write(dir.path(), "src/a.rs", "a");

    assert_eq!(relative_paths(dir.path(), &["**/*.rs"]), vec!["src/a.rs"]);
}

#[test]
fn hidden_files_are_still_matched() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "SECRET=1");

    assert_eq!(relative_paths(dir.path(), &[".env"]), vec![".env"]);
}

#[test]
fn a_pattern_matching_nothing_yields_an_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.txt", "a");

    assert_eq!(relative_paths(dir.path(), &["*.rs"]), Vec::<String>::new());
}

#[test]
fn outputs_are_satisfied_by_literal_existence_and_glob_matches() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "target/release/app", "binary");
    write(dir.path(), "dist/bundle.js", "js");

    let satisfied = |patterns: &[&str]| {
        let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
        outputs_satisfied(dir.path(), &patterns)
    };

    assert!(satisfied(&["target/release/app"]));
    assert!(satisfied(&["dist/*.js"]));
    assert!(satisfied(&[]), "no declared outputs means nothing to check");
    assert!(!satisfied(&["target/release/missing"]));
    assert!(!satisfied(&["dist/*.css"]));
}

#[test]
fn outputs_check_ignores_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "target/\n");
    write(dir.path(), "target/release/app", "binary");

    let patterns = vec!["target/*/app".to_string()];
    assert!(
        outputs_satisfied(dir.path(), &patterns),
        "outputs live in ignored directories; the check must query the raw disk"
    );
}
```

- [ ] **Step 3: Run the tests, verify they fail to compile**

Run: `cargo test -p alba-core --test files`
Expected: FAIL — `expand_globs` and `outputs_satisfied` are not defined.

- [ ] **Step 4: Implement `crates/alba-core/src/files.rs`**

```rust
//! Filesystem resolution for `inputs`/`outputs` declarations: which files
//! a beam's glob patterns actually name on disk.
//!
//! The two functions deliberately answer different questions against
//! different views of the disk. [`expand_globs`] resolves `inputs` for
//! fingerprinting and is `.gitignore`-aware: hashing `src/**/*` must not
//! aspirate build artifacts a `.gitignore` already declares uninteresting.
//! [`outputs_satisfied`] checks `outputs` for presence and queries the raw
//! disk: outputs typically live in exactly those ignored directories
//! (`target/`, `dist/`), so filtering them would make their patterns never
//! match.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The files under `base` matching any of `patterns`, as
/// `(relative path with '/' separators, absolute path)` pairs sorted by
/// relative path — sorted so a fingerprint built from the list is
/// deterministic across platforms and directory iteration orders.
///
/// `.gitignore` files are honored even outside a git repository
/// (`require_git(false)`), so behaviour does not silently change when a
/// project is unpacked from a tarball. Hidden files are included —
/// `inputs [".env"]` must work — but `.git/` and `.alba/` are never
/// walked: the first is noise, and hashing the cache's own directory
/// would invalidate every beam on every run.
///
/// Never fails: patterns were validated when the Beamfile loaded, and a
/// pattern this function still cannot compile yields an empty list, which
/// the cache treats as "nothing matched".
pub fn expand_globs(base: &Path, patterns: &[String]) -> Vec<(String, PathBuf)> {
    let Some(set) = build_globset(patterns) else {
        return Vec::new();
    };

    let walker = ignore::WalkBuilder::new(base)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| {
            entry.file_name() != OsStr::new(".git") && entry.file_name() != OsStr::new(".alba")
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(base) else {
            continue;
        };
        let unified = relative.to_string_lossy().replace('\\', "/");
        if set.is_match(&unified) {
            files.push((unified, entry.into_path()));
        }
    }
    files.sort();
    files
}

/// Whether every `outputs` pattern is satisfied on disk: a literal path
/// must exist, a glob pattern must match at least one entry. Raw disk, no
/// `.gitignore` filtering — see the module doc comment.
pub fn outputs_satisfied(base: &Path, patterns: &[String]) -> bool {
    patterns.iter().all(|pattern| {
        if pattern.contains(['*', '?', '[']) {
            glob::glob(&base.join(pattern).to_string_lossy())
                .map(|mut matches| matches.any(|entry| entry.is_ok()))
                .unwrap_or(false)
        } else {
            base.join(pattern).exists()
        }
    })
}

fn build_globset(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(globset::Glob::new(pattern).ok()?);
    }
    builder.build().ok()
}
```

In `crates/alba-core/src/lib.rs`, add `mod files;` alongside the existing modules and `pub use files::{expand_globs, outputs_satisfied};` alongside the existing re-exports.

- [ ] **Step 5: Run the tests until green, plus clippy/fmt**

Run: `cargo test -p alba-core --test files && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/alba-core
git commit -m "✨ feat(core): resolve input and output globs on disk"
```

---

### Task 2: Fingerprint hashing (`alba-engine`)

**Files:**
- Modify: `crates/alba-engine/Cargo.toml` (add `blake3.workspace = true`)
- Create: `crates/alba-engine/src/cache/mod.rs`
- Create: `crates/alba-engine/src/cache/fingerprint.rs`
- Modify: `crates/alba-engine/src/lib.rs` (add `mod cache;`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces (all `pub(crate)`, used by Task 5):
  - `struct BeamFacts<'a> { files: &'a [(String, String)], commands: &'a [String], env: &'a [(String, String)], args: &'a [String], needs: &'a [String] }`
  - `fn fingerprint(facts: &BeamFacts<'_>) -> String` — blake3 hex over all fields, length-prefixed.
  - `fn static_contribution(commands: &[String], env: &[(String, String)], args: &[String]) -> String` — what a non-cacheable beam contributes to its dependents' fingerprints.
  - `fn hash_file(path: &Path) -> std::io::Result<String>` — blake3 hex of a file's content.

- [ ] **Step 1: Add the dependency**

In `crates/alba-engine/Cargo.toml` `[dependencies]`: `blake3.workspace = true`.

- [ ] **Step 2: Write the failing tests**

Create `crates/alba-engine/src/cache/fingerprint.rs` with the tests first (the implementation stubs do not exist yet, so this fails to compile — that is the red step):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn facts_fingerprint(
        files: &[(&str, &str)],
        commands: &[&str],
        env: &[(&str, &str)],
        args: &[&str],
        needs: &[&str],
    ) -> String {
        fingerprint(&BeamFacts {
            files: &pairs(files),
            commands: &strings(commands),
            env: &pairs(env),
            args: &strings(args),
            needs: &strings(needs),
        })
    }

    #[test]
    fn identical_facts_produce_identical_fingerprints() {
        let a = facts_fingerprint(&[("src/a.rs", "h1")], &["build"], &[("K", "v")], &[], &[]);
        let b = facts_fingerprint(&[("src/a.rs", "h1")], &["build"], &[("K", "v")], &[], &[]);
        assert_eq!(a, b);
    }

    #[test]
    fn every_field_participates_in_the_fingerprint() {
        let base = facts_fingerprint(&[("a", "h1")], &["cmd"], &[("K", "v")], &["arg"], &["n1"]);

        let variants = [
            facts_fingerprint(&[("a", "h2")], &["cmd"], &[("K", "v")], &["arg"], &["n1"]),
            facts_fingerprint(&[("b", "h1")], &["cmd"], &[("K", "v")], &["arg"], &["n1"]),
            facts_fingerprint(&[("a", "h1")], &["cmd2"], &[("K", "v")], &["arg"], &["n1"]),
            facts_fingerprint(&[("a", "h1")], &["cmd"], &[("K", "w")], &["arg"], &["n1"]),
            facts_fingerprint(&[("a", "h1")], &["cmd"], &[("K", "v")], &["other"], &["n1"]),
            facts_fingerprint(&[("a", "h1")], &["cmd"], &[("K", "v")], &["arg"], &["n2"]),
        ];
        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    /// Length-prefixing is what makes `["ab"]` and `["a", "b"]` distinct;
    /// plain concatenation would collide them.
    #[test]
    fn adjacent_items_cannot_collide_by_concatenation() {
        let joined = facts_fingerprint(&[], &["ab"], &[], &[], &[]);
        let split = facts_fingerprint(&[], &["a", "b"], &[], &[], &[]);
        assert_ne!(joined, split);
    }

    #[test]
    fn the_static_contribution_ignores_files_and_needs() {
        let contribution = static_contribution(&strings(&["cmd"]), &pairs(&[("K", "v")]), &[]);
        assert_eq!(
            contribution,
            facts_fingerprint(&[], &["cmd"], &[("K", "v")], &[], &[])
        );
    }

    #[test]
    fn hash_file_reflects_content_not_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input.txt");

        std::fs::write(&path, "v1").unwrap();
        let first = hash_file(&path).unwrap();
        std::fs::write(&path, "v2").unwrap();
        let second = hash_file(&path).unwrap();
        std::fs::write(&path, "v1").unwrap();
        let third = hash_file(&path).unwrap();

        assert_ne!(first, second);
        assert_eq!(first, third);
    }

    #[test]
    fn hash_file_reports_a_missing_file_as_an_error() {
        assert!(hash_file(std::path::Path::new("/does/not/exist")).is_err());
    }
}
```

`tempfile` must be in `crates/alba-engine/Cargo.toml` `[dev-dependencies]`: add `tempfile.workspace = true`.

Create `crates/alba-engine/src/cache/mod.rs`:

```rust
//! The skip-only cache: fingerprint a ready beam, decide hit or miss, and
//! persist the last successful run's manifest and logs under
//! `.alba/cache/`. Sub-modules: `fingerprint` (hashing), `store`
//! (persistence, Task 3). The scheduler is the only consumer.

mod fingerprint;

pub(crate) use fingerprint::{BeamFacts, fingerprint, hash_file, static_contribution};
```

And in `crates/alba-engine/src/lib.rs`, after the existing `mod` declarations: `mod cache;`.

- [ ] **Step 3: Run tests, verify failure**

Run: `cargo test -p alba-engine fingerprint`
Expected: FAIL to compile — the types and functions are missing.

- [ ] **Step 4: Implement the module (above the tests, same file)**

```rust
//! Fingerprinting: one blake3 hash per beam over everything that must
//! invalidate its cache entry. Every item is length-prefixed before it is
//! fed to the hasher, so adjacent strings cannot collide by concatenation
//! (`["ab"]` versus `["a", "b"]`), and every section is preceded by its
//! name so an empty section still leaves a trace.

use std::io;
use std::path::Path;

/// Everything that participates in a beam's fingerprint. `files` is the
/// sorted `(relative path, content hash)` list `expand_globs` + [`hash_file`]
/// produce; `needs` is each dependency's contribution, in `needs` order.
pub(crate) struct BeamFacts<'a> {
    pub files: &'a [(String, String)],
    pub commands: &'a [String],
    pub env: &'a [(String, String)],
    pub args: &'a [String],
    pub needs: &'a [String],
}

/// The blake3 hex fingerprint of `facts`. Any change to how this feeds
/// the hasher must bump `store::FORMAT_VERSION` (Task 3): an old manifest
/// compared against a new recipe would be silently meaningless.
pub(crate) fn fingerprint(facts: &BeamFacts<'_>) -> String {
    let mut hasher = blake3::Hasher::new();

    item(&mut hasher, "files");
    for (path, hash) in facts.files {
        item(&mut hasher, path);
        item(&mut hasher, hash);
    }
    item(&mut hasher, "commands");
    for command in facts.commands {
        item(&mut hasher, command);
    }
    item(&mut hasher, "env");
    for (name, value) in facts.env {
        item(&mut hasher, name);
        item(&mut hasher, value);
    }
    item(&mut hasher, "args");
    for arg in facts.args {
        item(&mut hasher, arg);
    }
    item(&mut hasher, "needs");
    for need in facts.needs {
        item(&mut hasher, need);
    }

    hasher.finalize().to_hex().to_string()
}

/// What a beam that cannot be cached (no declared `inputs`) contributes to
/// its dependents' fingerprints: its static parts only. This keeps a
/// non-cacheable dependency from poisoning the cascade — if its actual
/// output changes, the dependent's own `inputs` catch that by content.
pub(crate) fn static_contribution(
    commands: &[String],
    env: &[(String, String)],
    args: &[String],
) -> String {
    fingerprint(&BeamFacts {
        files: &[],
        commands,
        env,
        args,
        needs: &[],
    })
}

/// The blake3 hex hash of a file's content. Reads the whole file: inputs
/// are source files, and blake3 hashes gigabytes per second — streaming
/// would be complexity without a case that needs it yet.
pub(crate) fn hash_file(path: &Path) -> io::Result<String> {
    Ok(blake3::hash(&std::fs::read(path)?).to_hex().to_string())
}

fn item(hasher: &mut blake3::Hasher, text: &str) {
    hasher.update(&(text.len() as u64).to_le_bytes());
    hasher.update(text.as_bytes());
}
```

- [ ] **Step 5: Run tests until green, plus clippy/fmt**

Run: `cargo test -p alba-engine fingerprint && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS. Note: `cargo clippy` may flag the still-unused `pub(crate)` re-exports; if it does, add `#[allow(dead_code)]` on the `pub(crate) use` line in `cache/mod.rs` with a `// consumed by the scheduler in a follow-up change` comment, and remove it in Task 5.

- [ ] **Step 6: Commit**

```bash
git add crates/alba-engine
git commit -m "✨ feat(engine): fingerprint a beam's cacheable facts"
```

---

### Task 3: Cache store (`alba-engine`)

**Files:**
- Modify: `crates/alba-engine/Cargo.toml` (add `serde.workspace = true`, `serde_json.workspace = true`)
- Create: `crates/alba-engine/src/cache/store.rs`
- Modify: `crates/alba-engine/src/cache/mod.rs` (add `mod store;` and re-exports)

**Interfaces:**
- Consumes: `alba_core::BeamId`, `alba_executors::{OutputLine, Stream}`.
- Produces (all `pub(crate)`, used by Task 5 and 6):
  - `const FORMAT_VERSION: u32 = 1;`
  - `struct Manifest { version: u32, fingerprint: String, outputs: Vec<String>, duration_ms: u64 }` (derives `Debug, Clone, PartialEq, Serialize, Deserialize`)
  - `struct CacheStore` with:
    - `fn new(dir: PathBuf) -> Self`
    - `fn load(&self, id: &BeamId) -> Option<Manifest>` — `None` on missing, unreadable, corrupted, or version-mismatched manifest.
    - `fn store(&self, id: &BeamId, manifest: &Manifest, logs: &[OutputLine])` — best-effort, atomic (temp file + rename), silently ignores I/O errors (the cache must never fail a run).
    - `fn load_logs(&self, id: &BeamId) -> Vec<OutputLine>` — the stored log lines; empty on any error, bad lines skipped.

- [ ] **Step 1: Add dependencies**

In `crates/alba-engine/Cargo.toml` `[dependencies]`: `serde.workspace = true` and `serde_json.workspace = true`.

- [ ] **Step 2: Write the failing tests**

Create `crates/alba-engine/src/cache/store.rs` starting with its test module:

```rust
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
        assert_eq!(store_in(dir.path()).load(&BeamId("build".to_string())), None);
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

    #[test]
    fn missing_logs_load_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(store_in(dir.path()).load_logs(&BeamId("build".to_string())).is_empty());
    }
}
```

- [ ] **Step 3: Run tests, verify failure**

Run: `cargo test -p alba-engine store`
Expected: FAIL to compile.

- [ ] **Step 4: Implement the store (above the tests, same file)**

```rust
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

use alba_core::BeamId;
use alba_executors::{OutputLine, Stream};
use serde::{Deserialize, Serialize};

/// Bumped whenever the manifest layout *or the fingerprint recipe*
/// changes incompatibly: a manifest from another version is a miss, which
/// re-runs the beam and rewrites the entry — no migration, ever.
pub(crate) const FORMAT_VERSION: u32 = 1;

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
/// observes a half-written entry. The temporary name appends to the full
/// file name (rather than replacing the extension) so the `.json` and
/// `.log` of one beam cannot collide on the same temporary path.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
```

Update `crates/alba-engine/src/cache/mod.rs`:

```rust
mod fingerprint;
mod store;

pub(crate) use fingerprint::{BeamFacts, fingerprint, hash_file, static_contribution};
pub(crate) use store::{CacheStore, FORMAT_VERSION, Manifest};
```

(Keep any temporary `#[allow(dead_code)]` from Task 2 covering both lines; Task 5 removes it.)

- [ ] **Step 5: Run tests until green, plus clippy/fmt**

Run: `cargo test -p alba-engine store && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/alba-engine
git commit -m "✨ feat(engine): persist cache manifests and logs"
```

---

### Task 4: The cached status through events, summary, and renderers

**Files:**
- Modify: `crates/alba-engine/src/event.rs`
- Modify: `crates/alba-engine/src/scheduler.rs` (two mechanical touches)
- Modify: `crates/alba-cli/src/render/mod.rs`
- Modify: `crates/alba-cli/src/render/interleaved.rs`
- Modify: `crates/alba-cli/src/render/grouped.rs`
- Modify: `crates/alba-cli/src/render/json.rs`
- Modify: `crates/alba-engine/tests/scheduler.rs` (pattern updates only)

**Interfaces:**
- Consumes: existing `RunEvent`, `BeamStatus`, `RunSummary`.
- Produces (used by Tasks 5–7):
  - `RunEvent::BeamCached { id: BeamId }` — emitted instead of `BeamStarted` for a hit; renderers print the "cached" announcement on it.
  - `RunEvent::BeamOutput` gains a `replayed: bool` field (false for live output).
  - `BeamStatus::Cached` — counts as satisfied for dependents, exit code 0.
  - `RunSummary` gains `pub cached: Vec<BeamId>`; `record` maps `Cached` into it.
  - CLI: `status_label(&BeamStatus::Cached) == "cached"`; summary line shows `↺ N cached`; JSON emits `{"event":"beam_cached",...}`, `"replayed":bool` on outputs, `"cached"` status, and a `cached` bucket in `run_finished`.

- [ ] **Step 1: Write the failing CLI unit tests**

In `crates/alba-cli/src/render/mod.rs` tests, add:

```rust
#[test]
fn a_cached_beam_is_labelled_cached() {
    assert_eq!(status_label(&BeamStatus::Cached), "cached");
}

#[test]
fn the_summary_counts_cached_beams() {
    let summary = RunSummary {
        succeeded: vec![BeamId("a".to_string())],
        cached: vec![BeamId("b".to_string()), BeamId("c".to_string())],
        duration: Duration::from_millis(4100),
        ..RunSummary::default()
    };

    assert_eq!(
        summary_line(&summary),
        "\u{2713} 1 succeeded \u{b7} \u{21ba} 2 cached \u{b7} 4.1s"
    );
}
```

In `crates/alba-cli/src/render/json.rs` tests, add:

```rust
#[test]
fn a_cached_beam_emits_its_own_event_and_status() {
    let cached = json(&RunEvent::BeamCached {
        id: BeamId("gen".to_string()),
    });
    assert_eq!(cached["event"], "beam_cached");
    assert_eq!(cached["beam"], "gen");

    let finished = json(&RunEvent::BeamFinished {
        id: BeamId("gen".to_string()),
        status: BeamStatus::Cached,
        duration: std::time::Duration::from_millis(2100),
    });
    assert_eq!(finished["status"], "cached");
    assert!(finished["exit_code"].is_null());
}

#[test]
fn output_lines_carry_the_replayed_marker() {
    let event = |replayed| RunEvent::BeamOutput {
        id: BeamId("gen".to_string()),
        line: OutputLine {
            stream: Stream::Stdout,
            text: "generated".to_string(),
        },
        replayed,
    };

    assert_eq!(json(&event(true))["replayed"], true);
    assert_eq!(json(&event(false))["replayed"], false);
}

#[test]
fn the_final_event_carries_the_cached_bucket() {
    let event = RunEvent::RunFinished {
        summary: RunSummary {
            cached: vec![BeamId("gen".to_string())],
            ..RunSummary::default()
        },
    };
    assert_eq!(json(&event)["cached"][0], "gen");
}
```

- [ ] **Step 2: Run tests, verify failure**

Run: `cargo test -p alba-cli`
Expected: FAIL to compile — `BeamStatus::Cached`, `RunSummary::cached`, `RunEvent::BeamCached`, and the `replayed` field do not exist.

- [ ] **Step 3: Implement across engine and CLI**

`crates/alba-engine/src/event.rs`:

- Add to `RunEvent` (after `BeamStarted`), and extend `BeamOutput`:

```rust
    /// A cache hit: emitted instead of `BeamStarted` for a beam that is
    /// skipped, followed by its replayed `BeamOutput` lines and a
    /// `BeamFinished` with [`BeamStatus::Cached`] carrying the original
    /// run's duration.
    BeamCached {
        id: BeamId,
    },
    BeamOutput {
        id: BeamId,
        line: OutputLine,
        /// True for a line replayed from the cache rather than produced
        /// by a live command — CI tooling parsing the JSON stream needs
        /// to tell the two apart.
        replayed: bool,
    },
```

- Add `Cached` to `BeamStatus` (after `Succeeded`), with a doc line: a cached beam counts as satisfied for its dependents and as success for the exit code.
- Add `pub cached: Vec<BeamId>` to `RunSummary` (after `succeeded`), and `BeamStatus::Cached => self.cached.push(id),` to `record`. `exit_code` is unchanged (only `failed` matters).
- Update the `RunEvent` doc comment's per-beam ordering paragraph to mention the cached shape.

`crates/alba-engine/src/scheduler.rs` (mechanical):

- In `dependencies_satisfied`, extend the satisfied arm: `BeamStatus::Succeeded | BeamStatus::Cached | BeamStatus::FailedAllowed { .. } => {}`.
- In `forward_output`, the `RunEvent::BeamOutput` construction gains `replayed: false`.

`crates/alba-cli/src/render/mod.rs`:

- `status_label`: add `BeamStatus::Cached => "cached".to_string(),`.
- `summary_line`: insert `("\u{21ba}", summary.cached.len(), "cached"),` between the succeeded and failed entries.

`crates/alba-cli/src/render/interleaved.rs`, in `handle`:

```rust
RunEvent::BeamCached { id } => self.line(&id.0, "cached — replaying last output"),
RunEvent::BeamOutput { id, line, .. } => self.line(&id.0, &line.text),
```

and in the `BeamFinished` arm, mark a cached duration as historical:

```rust
let text = if matches!(status, alba_engine::BeamStatus::Cached) {
    format!("cached in {} (original run)", format_duration(*duration))
} else {
    format!("{} in {}", status_label(status), format_duration(*duration))
};
```

`crates/alba-cli/src/render/grouped.rs`, in `handle`:

- Add a `BeamCached` arm identical in effect to `BeamStarted` (`self.buffers.entry(id.0.clone()).or_default();`) — the hit still gets its group.
- `BeamOutput` arm: change the pattern to `RunEvent::BeamOutput { id, line, .. }` (replayed lines buffer like live ones; the text renderers do not distinguish).
- In the `BeamFinished` arm, build the header's status part the same way as interleaved: for `BeamStatus::Cached` use `format!("\u{2500}\u{2500} {} ({} original run, cached) \u{2500}\u{2500}", id.0, format_duration(*duration))`, otherwise the existing format.
- The test helper `output(id, text)` in this file's tests gains `replayed: false`.

`crates/alba-cli/src/render/json.rs`:

- `WireEvent` gains `BeamCached { beam: &'a str },`, `BeamOutput` gains `replayed: bool`, and `RunFinished` gains `cached: Vec<&'a str>,` (after `succeeded`).
- `From<&RunEvent>`: map `RunEvent::BeamCached { id }` to `WireEvent::BeamCached { beam: &id.0 }`; thread `replayed` through `BeamOutput`.
- `from_summary`: `cached: ids(&summary.cached),`.
- `status_name`: `BeamStatus::Cached => "cached",`; `exit_code`: add `Cached` to the `None` arm.
- Existing tests constructing `BeamOutput` gain `replayed: false`.

`crates/alba-engine/tests/scheduler.rs`: any pattern matching `RunEvent::BeamOutput { id, line }` becomes `RunEvent::BeamOutput { id, line, .. }`; any construction gains `replayed: false`. No behavioural changes.

- [ ] **Step 4: Run the whole workspace until green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS — including every pre-existing test.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "✨ feat: report cached beams through events and renderers"
```

---

### Task 5: Scheduler cache decision and manifest writes

**Files:**
- Modify: `crates/alba-engine/src/cache/mod.rs` (add `CacheOptions`)
- Modify: `crates/alba-engine/src/lib.rs` (re-export `CacheOptions`)
- Modify: `crates/alba-engine/src/scheduler.rs` (the core of this sub-project)
- Modify: `crates/alba-engine/tests/scheduler.rs` (existing `options()` helper gains `cache: None`)
- Modify: `crates/alba-cli/src/commands/run.rs` (add `cache: None` to its `RunOptions` so the workspace compiles; real wiring is Task 7)
- Test: `crates/alba-engine/tests/cache.rs` (new)

**Interfaces:**
- Consumes: Task 1 (`alba_core::{expand_globs, outputs_satisfied}`), Task 2 (`BeamFacts`, `fingerprint`, `hash_file`, `static_contribution`), Task 3 (`CacheStore`, `Manifest`, `FORMAT_VERSION`), Task 4 (`BeamCached`, `BeamStatus::Cached`, `RunSummary::cached`).
- Produces:
  - `pub struct CacheOptions { pub dir: PathBuf, pub force: bool }` (exported from `alba_engine`).
  - `RunOptions` gains `pub cache: Option<CacheOptions>` — `None` disables caching entirely.
  - Scheduler behaviour: a ready beam with a fingerprint hit emits `BeamCached` + `BeamFinished(Cached, original duration)` without executing; a successful miss writes its manifest. Log replay/store content arrives in Task 6.

- [ ] **Step 1: Write the failing behaviour tests (first batch)**

Create `crates/alba-engine/tests/cache.rs`:

```rust
//! Behaviour tests for the skip-only cache, driven through
//! [`alba_engine::run`] with a `FakeExecutor` and a real temporary
//! directory for both the input files and the `.alba/cache` state.
//!
//! `load_str` gives every beam `dir == "."`; each test rewrites the beams'
//! `dir` to its own temporary directory so glob expansion and the cache
//! never touch the process's working directory.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use alba_core::{BeamId, load_str};
use alba_engine::{BeamStatus, CacheOptions, RunEvent, RunOptions, RunSummary, run};
use alba_executors::{FakeBehavior, FakeExecutor};
use tokio_util::sync::CancellationToken;

struct Outcome {
    summary: RunSummary,
    events: Vec<RunEvent>,
    executor: Arc<FakeExecutor>,
}

impl Outcome {
    fn executed(&self) -> Vec<String> {
        self.executor.calls().into_iter().map(|c| c.command).collect()
    }
}

fn options(dir: &Path, force: bool) -> RunOptions {
    RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: Some(CacheOptions {
            dir: dir.join(".alba").join("cache"),
            force,
        }),
    }
}

fn write(dir: &Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn ids(bucket: &[BeamId]) -> Vec<&str> {
    bucket.iter().map(|id| id.0.as_str()).collect()
}

/// One run with a fresh executor, against a shared project dir (and thus
/// a shared cache). `params` are the target's positional arguments.
async fn run_once(
    source: &str,
    target: &str,
    dir: &Path,
    options: RunOptions,
    executor: FakeExecutor,
) -> Outcome {
    let mut project = load_str(source).expect("the test Beamfile must load");
    for beam in &mut project.beams {
        beam.dir = dir.to_path_buf();
    }
    let executor = Arc::new(executor);
    let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();

    let summary = run(
        &project,
        &BeamId(target.to_string()),
        options,
        executor.clone(),
        events_tx,
        CancellationToken::new(),
    )
    .await
    .expect("the run must not fail");

    let mut events = Vec::new();
    while let Ok(event) = events_rx.try_recv() {
        events.push(event);
    }
    Outcome { summary, events, executor }
}

const GEN: &str = r#"
beam gen {
  inputs ["data.txt"]
  run "generate"
}
"#;

#[tokio::test]
async fn a_second_unchanged_run_is_cached() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let first = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(first.executed().len(), 1);
    assert!(second.executed().is_empty(), "the second run must not execute");
    assert_eq!(ids(&second.summary.cached), vec!["gen"]);
    assert!(second.events.iter().any(
        |event| matches!(event, RunEvent::BeamCached { id } if id.0 == "gen")
    ));
    assert!(!second.events.iter().any(
        |event| matches!(event, RunEvent::BeamStarted { id } if id.0 == "gen")
    ));
}

#[tokio::test]
async fn a_changed_input_file_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    write(dir.path(), "data.txt", "v2");
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
    assert!(second.summary.cached.is_empty());
}

#[tokio::test]
async fn a_changed_command_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let changed = GEN.replace("generate", "generate --verbose");

    run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let second = run_once(&changed, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed(), vec!["generate --verbose"]);
}

#[tokio::test]
async fn a_changed_env_block_reruns() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let with_env = |value: &str| {
        format!(
            "beam gen {{\n  inputs [\"data.txt\"]\n  env {{ MODE = \"{value}\" }}\n  run \"generate\"\n}}\n"
        )
    };

    run_once(&with_env("debug"), "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let second = run_once(&with_env("release"), "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn changed_arguments_rerun() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    const PARAM: &str = r#"
beam deploy(target) {
  inputs ["data.txt"]
  run "deploy {target}"
}
"#;
    let with_args = |args: &[&str]| {
        let mut options = options(dir.path(), false);
        options.params = args.iter().map(|a| a.to_string()).collect();
        options
    };

    run_once(PARAM, "deploy", dir.path(), with_args(&["staging"]), FakeExecutor::new()).await;
    let same = run_once(PARAM, "deploy", dir.path(), with_args(&["staging"]), FakeExecutor::new()).await;
    let different = run_once(PARAM, "deploy", dir.path(), with_args(&["production"]), FakeExecutor::new()).await;

    assert!(same.executed().is_empty(), "same arguments must hit");
    assert_eq!(different.executed(), vec!["deploy production"]);
}

#[tokio::test]
async fn a_beam_without_inputs_always_runs() {
    let dir = tempfile::tempdir().unwrap();
    const NO_INPUTS: &str = r#"beam gen { run "generate" }"#;

    run_once(NO_INPUTS, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let second = run_once(NO_INPUTS, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
    assert!(second.summary.cached.is_empty());
}

#[tokio::test]
async fn no_cache_configuration_disables_caching() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    let disabled = || RunOptions {
        jobs: 4,
        keep_going: false,
        params: Vec::new(),
        cache: None,
    };

    run_once(GEN, "gen", dir.path(), disabled(), FakeExecutor::new()).await;
    let second = run_once(GEN, "gen", dir.path(), disabled(), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
}
```

- [ ] **Step 2: Write the failing behaviour tests (second batch, same file)**

```rust
#[tokio::test]
async fn missing_outputs_force_a_rerun() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    const WITH_OUTPUT: &str = r#"
beam gen {
  inputs ["data.txt"]
  outputs ["out.txt"]
  run "generate"
}
"#;
    // FakeExecutor touches no files, so the "produced" output is created
    // by hand: present for the second run, deleted before the third.
    write(dir.path(), "out.txt", "produced");

    run_once(WITH_OUTPUT, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let with_output = run_once(WITH_OUTPUT, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    std::fs::remove_file(dir.path().join("out.txt")).unwrap();
    let without_output = run_once(WITH_OUTPUT, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert!(with_output.executed().is_empty(), "outputs present: hit");
    assert_eq!(without_output.executed().len(), 1, "outputs missing: rerun");
}

#[tokio::test]
async fn a_failed_beam_writes_no_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let failing = FakeExecutor::new().on("generate", FakeBehavior { exit_code: 1, ..Default::default() });
    let first = run_once(GEN, "gen", dir.path(), options(dir.path(), false), failing).await;
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(ids(&first.summary.failed), vec!["gen"]);
    assert_eq!(second.executed().len(), 1, "a failure must not have cached");
}

#[tokio::test]
async fn an_allowed_failure_writes_no_manifest_either() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");
    const ALLOWED: &str = r#"
beam gen {
  allow_failure true
  inputs ["data.txt"]
  run "generate"
}
"#;

    let failing = FakeExecutor::new().on("generate", FakeBehavior { exit_code: 1, ..Default::default() });
    run_once(ALLOWED, "gen", dir.path(), options(dir.path(), false), failing).await;
    let second = run_once(ALLOWED, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn force_reruns_and_rewrites_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let forced = run_once(GEN, "gen", dir.path(), options(dir.path(), true), FakeExecutor::new()).await;
    let after = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(forced.executed().len(), 1, "--force must execute");
    assert!(forced.summary.cached.is_empty());
    assert!(after.executed().is_empty(), "the forced run must have rewritten the entry");
}

#[tokio::test]
async fn a_really_changed_dependency_invalidates_its_dependents() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "codegen.txt", "v1");
    write(dir.path(), "app.txt", "v1");
    const CHAIN: &str = r#"
beam codegen {
  inputs ["codegen.txt"]
  run "generate"
}

beam build {
  needs [codegen]
  inputs ["app.txt"]
  run "compile"
}
"#;

    run_once(CHAIN, "build", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let unchanged = run_once(CHAIN, "build", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    write(dir.path(), "codegen.txt", "v2");
    let changed = run_once(CHAIN, "build", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert!(unchanged.executed().is_empty(), "nothing changed: both cached");
    assert_eq!(ids(&unchanged.summary.cached), vec!["codegen", "build"]);
    assert_eq!(
        changed.executed(),
        vec!["generate", "compile"],
        "a changed dependency fingerprint must cascade to its dependents"
    );
}

#[tokio::test]
async fn a_non_cacheable_dependency_does_not_poison_its_dependents() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "app.txt", "v1");
    const MIXED: &str = r#"
beam always {
  run "prepare"
}

beam build {
  needs [always]
  inputs ["app.txt"]
  run "compile"
}
"#;

    run_once(MIXED, "build", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    let second = run_once(MIXED, "build", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed(), vec!["prepare"], "the input-less beam still runs");
    assert_eq!(ids(&second.summary.cached), vec!["build"]);
}

#[tokio::test]
async fn a_corrupted_cache_entry_is_a_miss() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;
    for entry in std::fs::read_dir(dir.path().join(".alba/cache")).unwrap() {
        std::fs::write(entry.unwrap().path(), "garbage").unwrap();
    }
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    assert_eq!(second.executed().len(), 1);
}

#[tokio::test]
async fn the_cached_duration_is_the_original_runs() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let slow = FakeExecutor::new().on(
        "generate",
        FakeBehavior { delay: Duration::from_millis(150), ..Default::default() },
    );
    run_once(GEN, "gen", dir.path(), options(dir.path(), false), slow).await;
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    let duration = second.events.iter().find_map(|event| match event {
        RunEvent::BeamFinished { status: BeamStatus::Cached, duration, .. } => Some(*duration),
        _ => None,
    });
    assert!(
        duration.expect("a cached BeamFinished must exist") >= Duration::from_millis(100),
        "the reported duration must be the original run's, not the replay's"
    );
}
```

- [ ] **Step 3: Run tests, verify failure**

Run: `cargo test -p alba-engine --test cache`
Expected: FAIL to compile — `CacheOptions` and `RunOptions::cache` do not exist.

- [ ] **Step 4: Implement**

`crates/alba-engine/src/cache/mod.rs` — add the public configuration type and export everything Task 2/3 defined (drop any leftover `#[allow(dead_code)]`):

```rust
use std::path::PathBuf;

/// Caller-facing cache configuration: where the state lives and whether
/// this run ignores it when reading. `force` still *writes*: a forced run
/// rewrites every successful beam's entry.
#[derive(Debug, Clone)]
pub struct CacheOptions {
    pub dir: PathBuf,
    pub force: bool,
}
```

`crates/alba-engine/src/lib.rs`: `pub use cache::CacheOptions;`.

`crates/alba-engine/src/scheduler.rs` — the substantive change. The shape:

1. `RunOptions` gains `pub cache: Option<CacheOptions>` (document: `None` disables caching — `alba check` and most tests).

2. The watch channels change payload from `Option<BeamStatus>` to `Option<BeamOutcome>`:

```rust
/// What a finished beam publishes to its dependents: how it ended, and
/// what it contributes to their fingerprints. `contribution` is `None`
/// when nothing stable could be computed (a render failure, a
/// cancellation) — a dependent seeing `None` from a satisfied dependency
/// is simply non-cacheable for that run.
#[derive(Debug, Clone)]
struct BeamOutcome {
    status: BeamStatus,
    contribution: Option<String>,
}
```

3. In `run()`, build the store once and hand each task a handle:

```rust
    let cache = options
        .cache
        .as_ref()
        .map(|options| Arc::new(CacheStore::new(options.dir.clone())));
    let force = options.cache.as_ref().is_some_and(|options| options.force);
```

`BeamTask` gains `cache: Option<Arc<CacheStore>>` and `force: bool`, filled per task with `cache.clone()` and `force`.

4. `dependencies_satisfied` becomes `wait_for_dependencies`, returning the contributions when satisfied:

```rust
/// Waits for every dependency and collects what each contributes to this
/// beam's fingerprint, in `needs` order. `None` means a dependency failed
/// or was cancelled — this beam must not run. `FailedAllowed` and
/// `Cached` both count as satisfied.
async fn wait_for_dependencies(
    dependencies: &mut [watch::Receiver<Option<BeamOutcome>>],
) -> Option<Vec<Option<String>>> {
    let mut contributions = Vec::with_capacity(dependencies.len());
    for dependency in dependencies {
        let outcome = wait_for_outcome(dependency).await;
        match outcome.status {
            BeamStatus::Succeeded | BeamStatus::Cached | BeamStatus::FailedAllowed { .. } => {
                contributions.push(outcome.contribution);
            }
            BeamStatus::Failed { .. } | BeamStatus::Cancelled => return None,
        }
    }
    Some(contributions)
}
```

`wait_for_status` becomes `wait_for_outcome`, same logic, returning a `BeamOutcome` (the dropped-sender and `None` fallbacks return `BeamOutcome { status: BeamStatus::Cancelled, contribution: None }`).

5. `run_beam` threads the contribution through:

```rust
async fn run_beam(mut task: BeamTask) -> BeamStatus {
    let (status, duration, contribution) = match wait_for_dependencies(&mut task.dependencies).await
    {
        Some(contributions) => process(&task, &contributions).await,
        None => (BeamStatus::Cancelled, Duration::ZERO, None),
    };

    if matches!(status, BeamStatus::Failed { .. }) && !task.keep_going {
        task.stop.cancel();
    }
    let _ = task.events.send(RunEvent::BeamFinished {
        id: task.beam.id.clone(),
        status: status.clone(),
        duration,
    });
    let _ = task.status.send(Some(BeamOutcome {
        status: status.clone(),
        contribution,
    }));
    status
}
```

6. The new `process` and `assess`:

```rust
/// What the cache concluded about a cacheable beam. `fingerprint` doubles
/// as this beam's contribution to its dependents; `hit` carries the
/// manifest to replay when the beam can be skipped.
struct Assessment {
    fingerprint: String,
    hit: Option<Manifest>,
}

/// Renders, consults the cache, and either replays a hit or executes.
///
/// Rendering happens here — before the cache decision, which needs the
/// rendered command — and the successful result is handed to `execute` so
/// commands are rendered exactly once. A template that fails to render is
/// deliberately *re*-rendered inside `execute`: the failure then follows
/// the exact event path it always has (started, stderr line, failed).
async fn process(
    task: &BeamTask,
    contributions: &[Option<String>],
) -> (BeamStatus, Duration, Option<String>) {
    let plan = render(&task.beam, &task.args).ok();
    let (assessment, notice) = assess(task, plan.as_ref(), contributions);

    if let Some(assessment) = &assessment {
        if let Some(manifest) = &assessment.hit {
            replay(task, manifest);
            return (
                BeamStatus::Cached,
                Duration::from_millis(manifest.duration_ms),
                Some(assessment.fingerprint.clone()),
            );
        }
    }

    let contribution = match (&assessment, &plan) {
        (Some(assessment), _) => Some(assessment.fingerprint.clone()),
        (None, Some(plan)) => Some(static_contribution(&plan.commands, &plan.env, &task.args)),
        (None, None) => None,
    };

    let (status, duration, lines) = execute(task, plan, notice).await;

    if status == BeamStatus::Succeeded {
        if let (Some(assessment), Some(store)) = (&assessment, &task.cache) {
            store.store(
                &task.beam.id,
                &Manifest {
                    version: FORMAT_VERSION,
                    fingerprint: assessment.fingerprint.clone(),
                    outputs: task.beam.outputs.clone(),
                    duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
                },
                &lines,
            );
        }
    }

    (status, duration, contribution)
}

/// The cache's verdict for this beam, plus an optional user-facing notice.
///
/// The verdict is `None` when the beam is not cacheable at all: caching
/// disabled, no declared `inputs`, a template that will not render, a
/// dependency with no stable contribution, or an input file that cannot
/// be hashed. Only that last case carries a notice — it is the one worth
/// telling the user about, emitted as a stderr line once the beam starts.
fn assess(
    task: &BeamTask,
    plan: Option<&RenderedBeam>,
    contributions: &[Option<String>],
) -> (Option<Assessment>, Option<String>) {
    let (Some(store), Some(plan)) = (task.cache.as_ref(), plan) else {
        return (None, None);
    };
    if task.beam.inputs.is_empty() {
        return (None, None);
    }
    let Some(needs) = contributions
        .iter()
        .cloned()
        .collect::<Option<Vec<String>>>()
    else {
        return (None, None);
    };

    let mut files = Vec::new();
    for (relative, absolute) in expand_globs(&task.beam.dir, &task.beam.inputs) {
        match hash_file(&absolute) {
            Ok(hash) => files.push((relative, hash)),
            // Unreadable between glob resolution and hashing (deleted,
            // permissions): non-cacheable this run, never a failed run.
            // Not covered by a test — an unreadable-but-present file has
            // no portable simulation — so keep this arm this simple.
            Err(error) => {
                return (
                    None,
                    Some(format!(
                        "cache: cannot hash input `{relative}` ({error}); running without cache"
                    )),
                );
            }
        }
    }

    let fingerprint = fingerprint(&BeamFacts {
        files: &files,
        commands: &plan.commands,
        env: &plan.env,
        args: &task.args,
        needs: &needs,
    });

    let hit = (!task.force)
        .then(|| store.load(&task.beam.id))
        .flatten()
        .filter(|manifest| manifest.fingerprint == fingerprint)
        .filter(|_| outputs_satisfied(&task.beam.dir, &task.beam.outputs));

    (Some(Assessment { fingerprint, hit }), None)
}

/// Announces a hit. Log replay arrives with the log store (Task 6); until
/// then a hit is the `BeamCached` announcement alone.
fn replay(task: &BeamTask, _manifest: &Manifest) {
    let _ = task.events.send(RunEvent::BeamCached {
        id: task.beam.id.clone(),
    });
}
```

One subtlety worth noting: on a hash failure `assess` returns `(None, notice)`, so the beam behaves exactly like a non-cacheable one — the `(None, Some(plan))` contribution arm in `process` gives its dependents its static parts, no manifest is written, and the notice reaches the user through `execute`.

7. `execute` changes signature to accept the plan and the notice, and returns the captured lines placeholder for Task 6 (empty for now):

```rust
async fn execute(
    task: &BeamTask,
    plan: Option<RenderedBeam>,
    notice: Option<String>,
) -> (BeamStatus, Duration, Vec<OutputLine>) {
```

Inside, everything stays as today except:
- After the `BeamStarted` event, if `notice` is `Some`, send it through `lines` as a `Stream::Stderr` `OutputLine`.
- The render step becomes: use the passed `plan` if `Some`; otherwise call `render(&task.beam, &task.args)` exactly as today (this is the re-render that reproduces the historical failure path).
- The early-return cancellation paths return `(BeamStatus::Cancelled, Duration::ZERO, Vec::new())`.
- The final return is `(status, started_at.elapsed(), Vec::new())` — the real capture lands in Task 6.

8. Imports at the top of `scheduler.rs`:

```rust
use alba_core::{
    Beam, BeamId, CoreError, ExecutorKind, Project, execution_subgraph, expand_globs,
    outputs_satisfied, render_template,
};

use crate::cache::{
    BeamFacts, CacheStore, FORMAT_VERSION, Manifest, fingerprint, hash_file, static_contribution,
};
```

9. `crates/alba-engine/tests/scheduler.rs`: the `options()` helper gains `cache: None`.

10. `crates/alba-cli/src/commands/run.rs`: the `RunOptions` literal gains `cache: None,` (temporary — Task 7 wires the real value).

- [ ] **Step 5: Run the whole workspace until green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS, including every pre-existing scheduler test.

- [ ] **Step 6: Commit**

```bash
git add crates
git commit -m "✨ feat(engine): skip beams whose fingerprint is unchanged"
```

---

### Task 6: Log capture and replay

**Files:**
- Modify: `crates/alba-engine/src/scheduler.rs` (`forward_output`, `execute`, `replay`)
- Test: `crates/alba-engine/tests/cache.rs` (extend)

**Interfaces:**
- Consumes: Task 5's `process`/`replay`/`execute` shape, Task 3's `CacheStore::{store, load_logs}`.
- Produces: a hit replays the stored `OutputLine`s as `BeamOutput { replayed: true }` events between `BeamCached` and `BeamFinished`; a successful miss stores its captured lines.

- [ ] **Step 1: Write the failing tests (extend `crates/alba-engine/tests/cache.rs`)**

```rust
#[tokio::test]
async fn a_hit_replays_the_stored_output_lines() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let talkative = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            output_lines: vec![
                alba_executors::OutputLine {
                    stream: alba_executors::Stream::Stdout,
                    text: "generated 12 files".to_string(),
                },
                alba_executors::OutputLine {
                    stream: alba_executors::Stream::Stderr,
                    text: "warning: deprecated".to_string(),
                },
            ],
            ..Default::default()
        },
    );
    run_once(GEN, "gen", dir.path(), options(dir.path(), false), talkative).await;
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    let replayed: Vec<(&str, bool)> = second
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::BeamOutput { line, replayed, .. } => Some((line.text.as_str(), *replayed)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replayed,
        vec![("generated 12 files", true), ("warning: deprecated", true)],
        "stored lines must replay, in order, marked as replayed"
    );
}

#[tokio::test]
async fn live_output_is_not_marked_replayed() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let talkative = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            output_lines: vec![alba_executors::OutputLine {
                stream: alba_executors::Stream::Stdout,
                text: "generated".to_string(),
            }],
            ..Default::default()
        },
    );
    let first = run_once(GEN, "gen", dir.path(), options(dir.path(), false), talkative).await;

    assert!(first.events.iter().any(|event| matches!(
        event,
        RunEvent::BeamOutput { replayed: false, .. }
    )));
    assert!(!first.events.iter().any(|event| matches!(
        event,
        RunEvent::BeamOutput { replayed: true, .. }
    )));
}

/// The replay order contract: `BeamCached`, then every replayed line,
/// then `BeamFinished` — mirroring a live beam's started/output/finished.
#[tokio::test]
async fn replayed_lines_sit_between_cached_and_finished() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.txt", "v1");

    let talkative = FakeExecutor::new().on(
        "generate",
        FakeBehavior {
            output_lines: vec![alba_executors::OutputLine {
                stream: alba_executors::Stream::Stdout,
                text: "generated".to_string(),
            }],
            ..Default::default()
        },
    );
    run_once(GEN, "gen", dir.path(), options(dir.path(), false), talkative).await;
    let second = run_once(GEN, "gen", dir.path(), options(dir.path(), false), FakeExecutor::new()).await;

    let index = |predicate: &dyn Fn(&RunEvent) -> bool| {
        second.events.iter().position(|e| predicate(e)).expect("event must exist")
    };
    let cached = index(&|e| matches!(e, RunEvent::BeamCached { .. }));
    let output = index(&|e| matches!(e, RunEvent::BeamOutput { replayed: true, .. }));
    let finished = index(&|e| matches!(e, RunEvent::BeamFinished { .. }));
    assert!(cached < output && output < finished);
}
```

- [ ] **Step 2: Run tests, verify failure**

Run: `cargo test -p alba-engine --test cache`
Expected: the three new tests FAIL (no lines are stored or replayed yet); everything else passes.

- [ ] **Step 3: Implement**

In `crates/alba-engine/src/scheduler.rs`:

- `forward_output` collects what it forwards and returns it:

```rust
/// Relabels an executor's output lines as this beam's output events until
/// the executor drops the last sender, and returns everything it saw —
/// the capture a successful beam stores for future replay.
async fn forward_output(
    id: BeamId,
    mut output: UnboundedReceiver<OutputLine>,
    events: UnboundedSender<RunEvent>,
) -> Vec<OutputLine> {
    let mut seen = Vec::new();
    while let Some(line) = output.recv().await {
        let _ = events.send(RunEvent::BeamOutput {
            id: id.clone(),
            line: line.clone(),
            replayed: false,
        });
        seen.push(line);
    }
    seen
}
```

- In `execute`, the forwarder join becomes the capture: `let lines = forwarder.await.unwrap_or_default();` and the final return is `(status, started_at.elapsed(), lines)`.
- `replay` gains the log half:

```rust
fn replay(task: &BeamTask, _manifest: &Manifest) {
    let _ = task.events.send(RunEvent::BeamCached {
        id: task.beam.id.clone(),
    });
    if let Some(store) = &task.cache {
        for line in store.load_logs(&task.beam.id) {
            let _ = task.events.send(RunEvent::BeamOutput {
                id: task.beam.id.clone(),
                line,
                replayed: true,
            });
        }
    }
}
```

- [ ] **Step 4: Run the whole workspace until green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-engine
git commit -m "✨ feat(engine): replay a cached beam's stored logs"
```

---

### Task 7: CLI wiring — cache on by default, `--force`, end-to-end tests

**Files:**
- Modify: `crates/alba-cli/src/args.rs` (`RunFlags` gains `force`)
- Modify: `crates/alba-cli/src/main.rs` (pass the Beamfile path to `commands::run::run`)
- Modify: `crates/alba-cli/src/commands/run.rs` (build `CacheOptions`)
- Test: `crates/alba-cli/tests/cli_run.rs` (extend)

**Interfaces:**
- Consumes: `alba_engine::CacheOptions`, `RunOptions::cache` (Task 5).
- Produces:
  - `alba run` caches by default under `<beamfile dir>/.alba/cache`; `--force` sets `CacheOptions::force`.
  - `commands::run::run` signature becomes `pub fn run(project: &Project, sources: &SourceMap, beamfile: &Path, target: &BeamId, params: Vec<String>, flags: &RunFlags) -> i32` (Task 8's `main.rs` restructure relies on `beamfile` already being resolved in `main.rs`, which it is today).

- [ ] **Step 1: Write the failing end-to-end tests (extend `crates/alba-cli/tests/cli_run.rs`)**

```rust
/// The whole cache loop through the real binary: a first run executes and
/// stores, an unchanged second run is cached and replays the output, a
/// changed input runs again, and `--force` bypasses the read side.
#[test]
fn an_unchanged_run_is_cached_and_replays_its_output() {
    let dir = project("beam gen { inputs [\"data.txt\"] run \"echo generated\" }\n");
    std::fs::write(dir.path().join("data.txt"), "v1").unwrap();

    let run = |extra: &[&str]| {
        let mut args = vec!["run", "gen"];
        args.extend_from_slice(extra);
        let assert = alba().current_dir(&dir).args(args).assert().success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    };

    let first = run(&[]);
    assert!(!first.contains("cached"), "a first run cannot be cached:\n{first}");
    assert!(first.contains("generated"));
    assert!(dir.path().join(".alba/cache").is_dir(), "the store must exist after a success");

    let second = run(&[]);
    assert!(second.contains("cached"), "unchanged inputs must hit:\n{second}");
    assert!(second.contains("generated"), "the stored output must replay:\n{second}");

    std::fs::write(dir.path().join("data.txt"), "v2").unwrap();
    let third = run(&[]);
    assert!(!third.contains("cached"), "a changed input must rerun:\n{third}");

    let forced = run(&["--force"]);
    assert!(!forced.contains("cached"), "--force must execute:\n{forced}");
}

/// The JSON stream's cache contract: a `beam_cached` event, and replayed
/// output lines marked `"replayed": true`.
#[test]
fn the_json_stream_marks_cached_beams_and_replayed_lines() {
    let dir = project("beam gen { inputs [\"data.txt\"] run \"echo generated\" }\n");
    std::fs::write(dir.path().join("data.txt"), "v1").unwrap();

    let run_json = || {
        let assert = alba()
            .current_dir(&dir)
            .args(["run", "gen", "--log-format", "json"])
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        stdout
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>()
    };

    let first = run_json();
    assert!(first.iter().any(|event| event["event"] == "beam_output"
        && event["replayed"] == false));

    let second = run_json();
    assert!(second.iter().any(|event| event["event"] == "beam_cached"));
    assert!(second.iter().any(|event| event["event"] == "beam_output"
        && event["replayed"] == true
        && event["text"] == "generated"));
    let finished = second
        .iter()
        .find(|event| event["event"] == "run_finished")
        .expect("the stream must end with run_finished");
    assert_eq!(finished["cached"][0], "gen");
}
```

Note: `serde_json` is already a dependency of `alba-cli` itself, but for use inside the *test* it must also be a dev-dependency. Add `serde_json.workspace = true` to `[dev-dependencies]` in `crates/alba-cli/Cargo.toml`.

- [ ] **Step 2: Run tests, verify failure**

Run: `cargo test -p alba-cli --test cli_run cached`
Expected: FAIL — the second run executes again (no cache is wired), and `--force` is an unknown flag.

- [ ] **Step 3: Implement**

`crates/alba-cli/src/args.rs`, in `RunFlags`:

```rust
    /// Ignore the cache when reading: run every beam, and rewrite the
    /// cache entries of the ones that succeed
    #[arg(long)]
    pub force: bool,
```

`crates/alba-cli/src/main.rs`: both `commands::run::run(...)` call sites gain `&beamfile` as the third argument (after `sources`).

`crates/alba-cli/src/commands/run.rs`:

- `run` and `execute` gain a `beamfile: &Path` parameter (threaded through; add `use std::path::Path;`).
- The `RunOptions` literal becomes:

```rust
    let options = RunOptions {
        jobs: jobs(flags.jobs),
        keep_going: flags.keep_going,
        params,
        cache: Some(CacheOptions {
            dir: cache_dir(beamfile),
            force: flags.force,
        }),
    };
```

with the helper (also used by Task 8's `cache clean`, so make it `pub(crate)` here):

```rust
/// The cache directory for the project `beamfile` defines: `.alba/cache`
/// next to the Beamfile. A bare `Beamfile` path has an empty parent,
/// which means the current directory.
pub(crate) fn cache_dir(beamfile: &Path) -> std::path::PathBuf {
    beamfile
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(".alba")
        .join("cache")
}
```

and `use alba_engine::CacheOptions;` added to the imports.

- [ ] **Step 4: Run the whole workspace until green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS. Pre-existing e2e tests are unaffected: their Beamfiles declare no `inputs`, so nothing they run is ever cached.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-cli
git commit -m "✨ feat(cli): cache runs with a force escape hatch"
```

---

### Task 8: `alba cache clean`

**Files:**
- Modify: `crates/alba-cli/src/args.rs` (`Command::Cache`, `CacheCommand`)
- Modify: `crates/alba-cli/src/main.rs` (dispatch before loading)
- Create: `crates/alba-cli/src/commands/cache.rs`
- Modify: `crates/alba-cli/src/commands/mod.rs` (`pub mod cache;`)
- Test: `crates/alba-cli/tests/cli_check_list.rs` (extend — it is the home of the non-run subcommand tests)

**Interfaces:**
- Consumes: Task 7's `commands::run::cache_dir`.
- Produces: `alba cache clean` removes `<beamfile dir>/.alba/cache`, exits 0 (also when there was nothing to remove), exits 2 only on a real I/O failure. It needs a Beamfile to locate the project, but does not load it — a broken Beamfile must not block cleaning.

- [ ] **Step 1: Write the failing end-to-end tests (extend `crates/alba-cli/tests/cli_check_list.rs`)**

Reuse the file's existing helpers if it has `alba()`/`project()` equivalents; otherwise define local ones identical to `cli_run.rs`'s:

```rust
#[test]
fn cache_clean_removes_the_store_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Beamfile"),
        "beam gen { inputs [\"data.txt\"] run \"echo generated\" }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("data.txt"), "v1").unwrap();

    assert_cmd::Command::cargo_bin("alba")
        .unwrap()
        .current_dir(&dir)
        .args(["run", "gen"])
        .assert()
        .success();
    assert!(dir.path().join(".alba/cache").is_dir());

    let clean = || {
        assert_cmd::Command::cargo_bin("alba")
            .unwrap()
            .current_dir(&dir)
            .args(["cache", "clean"])
            .assert()
            .success()
    };
    clean();
    assert!(!dir.path().join(".alba/cache").exists());
    clean(); // nothing left to remove is still a success
}

/// A Beamfile that does not even parse must not block cleaning: the
/// command only needs the file's location, not its content.
#[test]
fn cache_clean_works_with_a_broken_beamfile() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Beamfile"), "beam { this is not valid").unwrap();

    assert_cmd::Command::cargo_bin("alba")
        .unwrap()
        .current_dir(&dir)
        .args(["cache", "clean"])
        .assert()
        .success();
}
```

- [ ] **Step 2: Run tests, verify failure**

Run: `cargo test -p alba-cli --test cli_check_list cache_clean`
Expected: FAIL — `cache` is an unknown subcommand.

- [ ] **Step 3: Implement**

`crates/alba-cli/src/args.rs`:

```rust
    /// Manage the project's cache.
    Cache {
        #[command(subcommand)]
        command: CacheCommand,
    },
```

added to `Command`, plus:

```rust
/// A subcommand of `alba cache`. `clean` is deliberately the only one for
/// now; the subcommand level exists so `alba cache status` can join it
/// without breaking the CLI's shape.
#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Remove every cache entry for this project.
    Clean,
}
```

`crates/alba-cli/src/main.rs`, in `run(cli)`, right after `resolve_beamfile` succeeds and *before* `load_project`:

```rust
    // `cache` needs the Beamfile only to locate the project, never its
    // content — a Beamfile that does not parse must not block cleaning
    // the cache next to it.
    if let Some(Command::Cache { command }) = &cli.command {
        return commands::cache::run(&beamfile, command);
    }
```

(import `CacheCommand` where `args::` types are imported if needed.)

Create `crates/alba-cli/src/commands/cache.rs`:

```rust
//! `alba cache <command>`: management of the on-disk cache. Dispatched in
//! `main.rs` *before* the Beamfile is loaded — see the call site.

use std::path::Path;

use crate::args::CacheCommand;
use crate::exit::EXIT_ALBA_ERROR;
use crate::render::LineSink;

pub fn run(beamfile: &Path, command: &CacheCommand) -> i32 {
    match command {
        CacheCommand::Clean => clean(beamfile),
    }
}

/// Removes the project's cache directory. A directory that does not exist
/// is a success — the user asked for there to be no cache, and there is
/// none.
fn clean(beamfile: &Path) -> i32 {
    let dir = super::run::cache_dir(beamfile);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            LineSink::stderr().line(&format!(
                "cannot clean the cache at {}: {error}",
                dir.display()
            ));
            EXIT_ALBA_ERROR
        }
    }
}
```

`crates/alba-cli/src/commands/mod.rs`: add `pub mod cache;`.

- [ ] **Step 4: Run the whole workspace until green, plus clippy/fmt**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/alba-cli
git commit -m "✨ feat(cli): add cache clean"
```

---

### Task 9: Performance guard for the cached path

**Files:**
- Create: `crates/alba-engine/tests/perf.rs`

**Interfaces:**
- Consumes: the public `alba_engine::run` + `CacheOptions` path (Task 5), `FakeExecutor`.
- Produces: nothing — a regression guard.

- [ ] **Step 1: Write the guard (it passes immediately if Tasks 1–6 are sound; its red step is conceptual — run it once with the threshold at `Duration::ZERO` to see it measure, then set the real threshold)**

```rust
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
        std::fs::write(dir.path().join(format!("file_{i:03}.txt")), "x".repeat(1024)).unwrap();
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
```

- [ ] **Step 2: Run it**

Run: `cargo test -p alba-engine --test perf -- --nocapture`
Expected: PASS with a comfortable margin. If the median approaches the budget on the development machine, investigate before committing — the point of the guard is that this path is cheap.

- [ ] **Step 3: Clippy/fmt, then commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add crates/alba-engine/tests/perf.rs
git commit -m "✅ test(engine): guard the cached-run overhead"
```

---

### Task 10: Dogfood the cache — Beamfile, `.gitignore`, README

**Files:**
- Modify: `Beamfile` (declare `inputs` on `fmt`, `lint`, `test`)
- Modify: `.gitignore` (ignore `.alba/`)
- Modify: `README.md` (a "Caching" section)

**Interfaces:**
- Consumes: everything — this is the spec's success criterion 1.
- Produces: the repository caches its own beams.

- [ ] **Step 1: Update the Beamfile**

Give the three real check beams inputs (the `check` umbrella beam and its `echo` stay input-less — an echo is cheaper than a hash):

```text
beam fmt {
  description "Check formatting"
  inputs ["crates/**/*.rs"]
  run "cargo fmt --check"
}

beam lint {
  description "Clippy with warnings denied"
  inputs ["crates/**/*.rs", "Cargo.toml", "Cargo.lock"]
  run "cargo clippy --all-targets -- -D warnings"
}

beam test {
  description "Run the test suite"
  inputs ["crates/**/*.rs", "Cargo.toml", "Cargo.lock"]
  run "cargo test --workspace"
}
```

(`build` already declares `inputs`/`outputs`; extend its `inputs` to `["crates/**/*.rs", "Cargo.toml", "Cargo.lock"]` for consistency.)

- [ ] **Step 2: Ignore the cache directory**

Append to `.gitignore`:

```text
# Alba's own cache
.alba/
```

- [ ] **Step 3: Document caching in the README**

Add a `## Caching` section after the existing usage documentation, in the README's established voice. It must state, in prose: beams with declared `inputs` are skipped when nothing they depend on changed (`cached` in the output, logs replayed from the last run); what invalidates an entry (input content, the rendered command, `env`, arguments, a dependency's fingerprint); that a beam without `inputs` always runs; that missing declared `outputs` force a rerun; `--force` and `alba cache clean`; and that `.alba/` should be added to the project's `.gitignore` (Alba never edits it itself).

- [ ] **Step 4: Verify the dogfood loop end to end**

```bash
cargo build
./target/debug/alba run fmt
./target/debug/alba run fmt
```

Expected: the second invocation prints `fmt`'s line with `cached` and replays the (empty or short) output, and the summary counts `1 cached`. Then confirm invalidation: `touch crates/alba-cli/src/main.rs` (or edit any tracked `.rs` file trivially and revert), rerun, and confirm it executes again.

- [ ] **Step 5: Full workspace check, then commit**

Run: `cargo test --workspace && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add Beamfile .gitignore README.md
git commit -m "📝 docs: document caching and dogfood it in the Beamfile"
```
