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
    /// `target` narrows the watched inputs to that beam's execution
    /// subgraph; `None` watches the whole project instead, as an affected
    /// session with no `within` beam does.
    pub(crate) fn new(
        project: &Project,
        target: Option<&BeamId>,
        sources: &SourceMap,
    ) -> Result<Self, CoreError> {
        let subgraph = target
            .map(|id| execution_subgraph(project, id))
            .transpose()?;
        let mut groups: Vec<InputGroup> = Vec::new();
        for beam in project.beams.iter().filter(|beam| {
            subgraph
                .as_ref()
                .is_none_or(|subgraph| subgraph.contains(&beam.id))
        }) {
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
        self.set = builder
            .build()
            .unwrap_or_else(|_| globset::GlobSet::empty());
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

#[cfg(test)]
mod tests {
    use super::*;
    use alba_core::{BeamId, load_project};

    /// A real project on disk: a Beamfile whose `build` beam watches
    /// `src/**/*.rs`, one matching source file, and two git-ignored
    /// artifacts. `target/gen.rs` fails the `src/**/*.rs` pattern outright
    /// (`target/` isn't under `src/`), so it is rejected by the cheap
    /// pre-filter alone. `src/generated/gen.rs` genuinely matches the
    /// pattern's shape, so rejecting it depends on `.gitignore` handling
    /// — only reachable once `classify` falls through to re-running
    /// `expand_globs`.
    ///
    /// Built with `Some("build")` rather than `None`, so only `build`'s
    /// subgraph is watched: `free`, which has no `inputs` at all, sits
    /// outside it and must not blank the set.
    fn project_on_disk() -> (tempfile::TempDir, WatchSet) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\nsrc/generated/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/generated")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("src/generated/gen.rs"), "generated").unwrap();
        std::fs::write(dir.path().join("target/gen.rs"), "generated").unwrap();
        std::fs::write(
            dir.path().join("Beamfile"),
            "beam build { inputs [\"src/**/*.rs\"] run \"echo build\" }\n\
             beam free { run \"echo free\" }\n",
        )
        .unwrap();

        let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
        let set = WatchSet::new(&project, Some(&BeamId("build".to_string())), &sources).unwrap();
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

    /// Both git-ignored artifacts are rejected, but for different reasons —
    /// see `project_on_disk`. `src/generated/gen.rs` is the case that
    /// actually proves `.gitignore` handling: it would classify as `Input`
    /// if `classify` answered from the pattern match alone, without ever
    /// consulting `expand_globs`.
    #[test]
    fn a_gitignored_file_never_triggers() {
        let (dir, mut set) = project_on_disk();
        for gitignored in ["target/gen.rs", "src/generated/gen.rs"] {
            assert!(matches!(
                set.classify(&dir.path().join(gitignored)),
                Relevance::Irrelevant
            ));
        }
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
        // src/lib.rs only: src/generated/gen.rs matches the pattern too, so
        // this count would be 2 if `expand_globs`'s own `.gitignore`
        // handling weren't what's actually doing the excluding.
        assert_eq!(set.file_count(), 1);
    }

    /// A `Some` target that names no beam in the project is a `CoreError`,
    /// not a silently empty set.
    #[test]
    fn an_unknown_target_is_a_core_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Beamfile"), "beam a { run \"echo a\" }\n").unwrap();
        let (project, sources) = load_project(&dir.path().join("Beamfile")).unwrap();
        assert!(WatchSet::new(&project, Some(&BeamId("missing".to_string())), &sources).is_err());
    }
}
