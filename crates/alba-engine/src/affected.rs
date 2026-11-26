//! Which beams a git diff touches: the `--affected <ref>` selection.
//!
//! [`affected_beams`] is the pure half, over a loaded project and a list
//! of absolute changed paths. [`select`] is the whole thing: it asks git
//! for the paths, then narrows the result to what one run should target.
//!
//! The same matching decides each beam's `git.changes()` mode
//! ([`beam_modes`]) against the change set [`resolve_changes`] computes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alba_core::git::{changed_files, changed_files_since, head, merge_base};
use alba_core::{
    Beam, BeamId, ChangeMode, FullReason, Project, SourceId, SourceMap, execution_subgraph,
};

use crate::EngineError;
use crate::scheduler::Targets;

/// What a run was asked for, before git (if involved) has answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// `alba run <beam>`: the beam and its subgraph.
    Beam(BeamId),
    /// `alba run --affected <ref> [beam]`: what changed since `reference`,
    /// narrowed to `within`'s subgraph when a beam was named.
    Affected {
        reference: String,
        within: Option<BeamId>,
    },
}

impl Selection {
    /// The beam whose subgraph a watch session puts under watch, or
    /// `None` for the whole project.
    pub fn watched_target(&self) -> Option<&BeamId> {
        match self {
            Selection::Beam(id) => Some(id),
            Selection::Affected { within, .. } => within.as_ref(),
        }
    }
}

/// Every beam affected by what changed since `reference`, in project
/// order: the beams whose `inputs` match a changed path or whose Beamfile
/// changed, closed over their dependents. `root` is the project root git
/// runs from; changes anywhere in its repository count.
pub fn affected(
    project: &Project,
    sources: &SourceMap,
    root: &Path,
    reference: &str,
) -> Result<Vec<BeamId>, EngineError> {
    let changed = changed_files(root, reference)?;
    Ok(affected_beams(project, sources, &changed))
}

/// [`affected`] narrowed to one run's targets. `Beam` is validated here
/// (an unknown beam is the same `CoreError` a plain run reports), so a
/// caller can rely on `select` for the exit-2-at-startup rule.
pub fn select(
    project: &Project,
    sources: &SourceMap,
    root: &Path,
    selection: &Selection,
) -> Result<Targets, EngineError> {
    select_with_changes(project, sources, root, selection, None)
}

/// [`select`], reusing the run's change set when it was computed against
/// the reference an affected selection names, so a run asks git for its
/// diff once, for the selection and for `git.changes()` alike. Any other
/// `changes` (none, or another reference) leaves [`select`] to ask git.
pub fn select_with_changes(
    project: &Project,
    sources: &SourceMap,
    root: &Path,
    selection: &Selection,
    changes: Option<&RunChanges>,
) -> Result<Targets, EngineError> {
    match selection {
        Selection::Beam(id) => {
            execution_subgraph(project, id)?;
            Ok(Targets::beam(id.clone()))
        }
        Selection::Affected { reference, within } => {
            let affected = match changes {
                Some(changes) if changes.reference == *reference => {
                    affected_beams(project, sources, &changes.changed)
                }
                _ => affected(project, sources, root, reference)?,
            };
            let beams = match within {
                // Closure over dependents means: if anything in the
                // subgraph is affected, so is its root. The subgraph then
                // runs as usual, the cache skipping what did not move.
                Some(id) => {
                    execution_subgraph(project, id)?;
                    if affected.contains(id) {
                        vec![id.clone()]
                    } else {
                        Vec::new()
                    }
                }
                // Nothing can bind a parameterized beam's arguments in a
                // run with several targets; `alba affected` still lists it.
                None => affected
                    .into_iter()
                    .filter(|id| {
                        project
                            .beams
                            .iter()
                            .any(|beam| beam.id == *id && beam.params.is_empty())
                    })
                    .collect(),
            };
            Ok(Targets {
                beams,
                affected_by: Some(reference.clone()),
            })
        }
    }
}

/// Where a run's git reference comes from: the caller's policy, read
/// against the project of each run, so a session that reloads its
/// Beamfile follows an edited `base`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceSource {
    /// No reference: every beam calling `git.changes()` runs in full mode.
    Off,
    /// The `base` the project's `git` block declares, if it declares one.
    Declared,
    /// A reference the caller named; it holds whatever the project says.
    Explicit(String),
}

impl ReferenceSource {
    /// The reference in effect for `project`, if any.
    pub fn reference<'a>(&'a self, project: &'a Project) -> Option<&'a str> {
        match self {
            ReferenceSource::Off => None,
            ReferenceSource::Declared => project.git.base.as_deref(),
            ReferenceSource::Explicit(reference) => Some(reference),
        }
    }
}

/// The change set one run computes against a reference, shared by every
/// beam that calls `git.changes()`.
#[derive(Debug, Clone)]
pub struct RunChanges {
    /// The reference in effect, as the caller gave it.
    pub reference: String,
    /// The full SHA of the merge base of `reference` and `HEAD`.
    pub merge_base: String,
    /// The merge base is `HEAD` and the working tree is clean: the run is
    /// on the target branch itself, with nothing of its own to narrow to.
    pub on_target_branch: bool,
    /// Every changed path of the repository since the merge base,
    /// untracked ones included, absolute and lexically normal, in the
    /// form the beams' directories are in (see [`resolve_changes`]).
    pub changed: Arc<Vec<PathBuf>>,
}

/// Computes the run's change set against `reference`: one merge base, one
/// diff and one untracked listing, plus the `HEAD` check for the target
/// branch. `root` is the project root, as the loader anchored the beams'
/// directories to it: the changed paths are joined onto it as given,
/// never onto a canonicalized form, so they compare equal to
/// `beam.dir`-anchored patterns even where the temporary directory is a
/// symbolic link (`/var` and `/private/var` on macOS).
///
/// Any git failure is returned: the caller decides whether it is a
/// warning and a full run (a declared `base`) or an error (an explicit
/// `--affected <ref>`).
pub fn resolve_changes(root: &Path, reference: &str) -> Result<RunChanges, EngineError> {
    let merge_base = merge_base(root, reference)?;
    let changed = changed_files_since(root, &merge_base)?;
    let head = head(root)?;
    Ok(RunChanges {
        reference: reference.to_string(),
        on_target_branch: head.sha == merge_base && !head.dirty,
        merge_base,
        changed: Arc::new(changed),
    })
}

/// Each beam's `git.changes()` mode against `changes`, for the beams that
/// call it (the others are absent). The rules apply in order, the first
/// one that holds deciding:
///
/// 1. on the target branch with a clean tree: full;
/// 2. the beam's own Beamfile changed: full;
/// 3. a beam it transitively `needs` is affected: full, naming the direct
///    dependency through which it is;
/// 4. a changed path matches one of its `inputs` but none of its
///    `git.changes()` globs (a `phpcs.xml` change): full, naming the path;
/// 5. otherwise incremental.
///
/// The case without a reference, or with one that did not resolve, never
/// reaches here: the scheduler gives those beams full mode directly.
pub(crate) fn beam_modes(
    project: &Project,
    sources: &SourceMap,
    changes: &RunChanges,
) -> HashMap<BeamId, ChangeMode> {
    let calling: Vec<&Beam> = project
        .beams
        .iter()
        .filter(|beam| !beam.change_globs.is_empty())
        .collect();
    if calling.is_empty() {
        return HashMap::new();
    }
    if changes.on_target_branch {
        return calling
            .into_iter()
            .map(|beam| {
                (
                    beam.id.clone(),
                    ChangeMode::Full(FullReason::OnTargetBranch),
                )
            })
            .collect();
    }

    let changed = changes.changed.as_slice();
    let beamfile_changed = beamfiles_changed(project, sources, changed, true);
    let mut directly: HashSet<&str> = beamfile_changed.clone();
    // Per beam, the changed paths matched by a `git.changes()` glob and
    // those matched by any other input.
    let mut in_globs: HashMap<&str, HashSet<&PathBuf>> = HashMap::new();
    let mut outside: HashMap<&str, Vec<&PathBuf>> = HashMap::new();
    for_each_input_match(project, changed, |beam, pattern, path| {
        directly.insert(beam.id.0.as_str());
        if beam.change_globs.iter().any(|glob| glob == pattern) {
            in_globs.entry(beam.id.0.as_str()).or_default().insert(path);
        } else {
            outside.entry(beam.id.0.as_str()).or_default().push(path);
        }
    });
    let affected = close_over_dependents(project, directly);
    let root = project_root(sources);

    calling
        .into_iter()
        .map(|beam| {
            let id = beam.id.0.as_str();
            let mode = if beamfile_changed.contains(id) {
                ChangeMode::Full(FullReason::BeamfileChanged)
            } else if let Some(need) = beam
                .needs
                .iter()
                // Deliberate, even for a dependency itself incremental on
                // its own sources: any doubt means full mode.
                .find(|need| affected.contains(need.value.0.as_str()))
            {
                ChangeMode::Full(FullReason::DependencyAffected(need.value.clone()))
            } else if let Some(path) = first_outside(&outside, &in_globs, id) {
                // `change_globs` also holds the globs of calls in a branch
                // that depends on a parameter, which may not run this
                // time. A path matching only such a glob then counts as
                // covered and the beam stays incremental: the list that
                // call would render is exactly the changed files, so
                // nothing is hidden from the tool, and no input outside
                // every `git.changes()` glob is missed.
                ChangeMode::Full(FullReason::InputOutsideChanges(display_path(
                    root.as_deref(),
                    path,
                )))
            } else {
                ChangeMode::Incremental
            };
            (beam.id.clone(), mode)
        })
        .collect()
}

/// The smallest changed path of beam `id` that one of its inputs matched
/// and none of its `git.changes()` globs did, if any.
fn first_outside<'a>(
    outside: &HashMap<&str, Vec<&'a PathBuf>>,
    in_globs: &HashMap<&str, HashSet<&PathBuf>>,
    id: &str,
) -> Option<&'a PathBuf> {
    let covered = in_globs.get(id);
    outside
        .get(id)?
        .iter()
        .filter(|path| !covered.is_some_and(|covered| covered.contains(*path)))
        .min()
        .copied()
}

/// The directory of the root Beamfile, absolute and lexically normal: what
/// a full-mode reason names a changed path relative to.
fn project_root(sources: &SourceMap) -> Option<PathBuf> {
    let (path, _) = sources.get(SourceId(0))?;
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    Some(alba_core::lexically_normal(path.parent()?))
}

/// `path` relative to `root` with `/` separators, or whole when it lies
/// outside it (or there is no root to speak of).
fn display_path(root: Option<&Path>, path: &Path) -> String {
    let shown = root
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path);
    shown.to_string_lossy().replace('\\', "/")
}

/// The pure computation: `changed` holds absolute paths (existing or not).
pub(crate) fn affected_beams(
    project: &Project,
    sources: &SourceMap,
    changed: &[PathBuf],
) -> Vec<BeamId> {
    let mut directly = beamfiles_changed(project, sources, changed, false);
    for_each_input_match(project, changed, |beam, _, _| {
        directly.insert(beam.id.0.as_str());
    });
    let affected = close_over_dependents(project, directly);
    project
        .beams
        .iter()
        .filter(|beam| affected.contains(beam.id.0.as_str()))
        .map(|beam| beam.id.clone())
        .collect()
}

/// The beams whose own Beamfile is among `changed`, and, when
/// `unknown_changed`, those whose Beamfile `sources` does not hold: the
/// mode of such a beam is in doubt, and doubt must never narrow a run,
/// whereas the affected selection has always left them out.
fn beamfiles_changed<'p>(
    project: &'p Project,
    sources: &SourceMap,
    changed: &[PathBuf],
    unknown_changed: bool,
) -> HashSet<&'p str> {
    // No filesystem touched, deliberately: `changed` and every `beam.dir`
    // are already absolute (`changed_files`' lexical paths and the loader's
    // own `std::path::absolute`, respectively), built from the same root.
    // Both sides are compared in `lexically_normal` form, since an import
    // that climbs out (`../shared/Beamfile`) keeps its `..` in the loader's
    // paths, so a lexical prefix match is both correct and syscall-free — unlike
    // `watch::set::normalize`, which exists to reconcile paths a live OS
    // watcher resolved through a symlink, a problem git-diff paths do not
    // have.
    let changed_set: HashSet<&PathBuf> = changed.iter().collect();
    project
        .beams
        .iter()
        .filter(|beam| match sources.get(beam.source) {
            Some((path, _)) => {
                let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
                changed_set.contains(&alba_core::lexically_normal(&path))
            }
            None => unknown_changed,
        })
        .map(|beam| beam.id.0.as_str())
        .collect()
}

/// Calls `on_match(beam, input, path)` for every changed path and every
/// input pattern of a beam that matches it.
fn for_each_input_match<'p, 'c>(
    project: &'p Project,
    changed: &'c [PathBuf],
    mut on_match: impl FnMut(&'p Beam, &'p str, &'c PathBuf),
) {
    // Patterns sharing an anchor (the common case: one Beamfile, no
    // imports, no `..`) share the same relative view of `changed`, and are
    // compiled into one `GlobSet` per anchor rather than one per beam:
    // compiling is what dominates, so matching each changed path once
    // against the combined set — instead of once per beam's own set —
    // is what keeps this linear rather than quadratic in beams.
    let mut groups: HashMap<PathBuf, Vec<(String, &'p Beam, &'p str)>> = HashMap::new();
    for beam in &project.beams {
        for pattern in &beam.inputs {
            let (anchor, rest) = alba_core::anchor_pattern(&beam.dir, pattern);
            groups
                .entry(alba_core::lexically_normal(&anchor))
                .or_default()
                .push((rest, beam, pattern.as_str()));
        }
    }
    for (base, patterns) in &groups {
        let mut builder = globset::GlobSetBuilder::new();
        let mut owners: Vec<(&'p Beam, &'p str)> = Vec::new();
        for (rest, beam, pattern) in patterns {
            if let Ok(glob) = globset::Glob::new(rest) {
                builder.add(glob);
                owners.push((beam, pattern));
            }
        }
        let Ok(set) = builder.build() else { continue };
        for path in changed {
            let Ok(relative) = path.strip_prefix(base) else {
                continue;
            };
            let unified = relative.to_string_lossy().replace('\\', "/");
            let candidate = globset::Candidate::new(&unified);
            for index in set.matches_candidate(&candidate) {
                let (beam, pattern) = owners[index];
                on_match(beam, pattern, path);
            }
        }
    }
}

/// `affected` closed over dependents: every beam that needs one of them,
/// transitively.
fn close_over_dependents<'p>(
    project: &'p Project,
    mut affected: HashSet<&'p str>,
) -> HashSet<&'p str> {
    // ponytail: fixpoint over the whole beam list, quadratic in beams; a
    // reverse adjacency map if a project ever has thousands of them.
    loop {
        let before = affected.len();
        for beam in &project.beams {
            if beam
                .needs
                .iter()
                .any(|need| affected.contains(need.value.0.as_str()))
            {
                affected.insert(beam.id.0.as_str());
            }
        }
        if affected.len() == before {
            return affected;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The spec's guard: on the order of ten milliseconds, git's own time
    /// excluded. 300 beams in a chain, each with an input pattern, against
    /// 2000 changed paths; the median of 15 samples, as the other guards do.
    ///
    /// Ignored by default, like the other wall-clock budgets: the suite
    /// running beside it is enough load to break it. CI runs it alone; run
    /// it locally with `cargo test -p alba-engine --lib -- --ignored affected_selection`.
    #[test]
    #[ignore = "timing guard: needs an idle machine; CI runs it in its own step"]
    fn affected_selection_stays_imperceptible() {
        let mut source = String::new();
        for i in 0..300 {
            let needs = if i == 0 {
                String::new()
            } else {
                format!("needs [b{}] ", i - 1)
            };
            source.push_str(&format!(
                "beam b{i} {{ {needs}inputs [\"src/m{i}/**\"] run \"x\" }}\n"
            ));
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Beamfile"), &source).unwrap();
        let (project, sources) = alba_core::load_project(&dir.path().join("Beamfile")).unwrap();
        let changed: Vec<PathBuf> = (0..2000)
            .map(|i| dir.path().join(format!("src/m{}/f{i}.rs", i % 300)))
            .collect();

        let mut samples: Vec<Duration> = (0..15)
            .map(|_| {
                let start = Instant::now();
                let affected = affected_beams(&project, &sources, &changed);
                assert_eq!(affected.len(), 300);
                start.elapsed()
            })
            .collect();
        samples.sort();
        let median = samples[samples.len() / 2];
        assert!(median < Duration::from_millis(50), "median {median:?}");
    }
}
