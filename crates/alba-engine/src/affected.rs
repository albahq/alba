//! Which beams a git diff touches: the `--affected <ref>` selection.
//!
//! [`affected_beams`] is the pure half, over a loaded project and a list
//! of absolute changed paths. [`select`] is the whole thing: it asks git
//! for the paths, then narrows the result to what one run should target.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use alba_core::git::changed_files;
use alba_core::{Beam, BeamId, Project, SourceMap, execution_subgraph};

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
/// runs from; paths git reports are relative to it.
pub fn affected(
    project: &Project,
    sources: &SourceMap,
    root: &Path,
    reference: &str,
) -> Result<Vec<BeamId>, EngineError> {
    let changed: Vec<PathBuf> = changed_files(root, reference)?
        .into_iter()
        .map(|path| root.join(path))
        .collect();
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
    match selection {
        Selection::Beam(id) => {
            execution_subgraph(project, id)?;
            Ok(Targets::beam(id.clone()))
        }
        Selection::Affected { reference, within } => {
            let affected = affected(project, sources, root, reference)?;
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

/// The pure computation: `changed` holds absolute paths (existing or not).
pub(crate) fn affected_beams(
    project: &Project,
    sources: &SourceMap,
    changed: &[PathBuf],
) -> Vec<BeamId> {
    // No filesystem touched, deliberately: `changed` and every `beam.dir`
    // are already absolute (the caller's `root.join(..)` and the loader's
    // own `std::path::absolute`, respectively), built from the same root,
    // so a lexical prefix match is both correct and syscall-free — unlike
    // `watch::set::normalize`, which exists to reconcile paths a live OS
    // watcher resolved through a symlink, a problem git-diff paths do not
    // have.
    let changed_set: HashSet<&PathBuf> = changed.iter().collect();
    let mut affected: HashSet<&str> = HashSet::new();

    for beam in &project.beams {
        let beamfile = sources
            .get(beam.source)
            .map(|(path, _)| std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()));
        if beamfile.is_some_and(|beamfile| changed_set.contains(&beamfile)) {
            affected.insert(beam.id.0.as_str());
        }
    }

    // Beams sharing a `dir` (the common case: one Beamfile, no imports)
    // share the same relative view of `changed`, and their patterns are
    // compiled into one `GlobSet` per `dir` rather than one per beam:
    // compiling is what dominates, so matching each changed path once
    // against the combined set — instead of once per beam's own set —
    // is what keeps this linear rather than quadratic in beams.
    let mut groups: HashMap<&Path, Vec<&Beam>> = HashMap::new();
    for beam in &project.beams {
        if !beam.inputs.is_empty() {
            groups.entry(beam.dir.as_path()).or_default().push(beam);
        }
    }
    for (base, beams) in groups {
        let mut builder = globset::GlobSetBuilder::new();
        let mut owners: Vec<&str> = Vec::new();
        for beam in &beams {
            for pattern in &beam.inputs {
                if let Ok(glob) = globset::Glob::new(pattern) {
                    builder.add(glob);
                    owners.push(beam.id.0.as_str());
                }
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
                affected.insert(owners[index]);
            }
        }
    }

    // Closure over dependents. ponytail: fixpoint over the whole beam
    // list, quadratic in beams; a reverse adjacency map if a project ever
    // has thousands of them.
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
            break;
        }
    }

    project
        .beams
        .iter()
        .filter(|beam| affected.contains(beam.id.0.as_str()))
        .map(|beam| beam.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The spec's guard: on the order of ten milliseconds, git's own time
    /// excluded. 300 beams in a chain, each with an input pattern, against
    /// 2000 changed paths; the median of 15 samples, as the other guards do.
    #[test]
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
