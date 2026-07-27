//! Dependency-graph validation and subgraph extraction, the last piece a
//! [`Project`] needs before an engine can schedule it: [`validate_graph`]
//! checks that every `needs` entry names a beam that actually exists and
//! that no beam (transitively) needs itself, and [`execution_subgraph`]
//! extracts the transitive closure of a target beam.
//!
//! [`validate_graph`] is called by [`crate::loader::load_project`] and
//! [`crate::eval::load_str`] before either hands a [`Project`] back to its
//! caller, so a `Project` that exists at all has already passed both
//! checks — [`execution_subgraph`] leans on that invariant (see its own
//! doc comment) rather than re-deriving it.
//!
//! ## Determinism
//!
//! Both functions build a `HashMap<&str, &Beam>` for O(1) id lookups, but
//! never iterate it: every loop that could affect *which* error gets
//! reported, or the DFS visiting order, walks [`Project::beams`] (a `Vec`,
//! in the order beams were declared) instead. A `HashMap`'s iteration
//! order is randomized per instance, so leaning on it here would make
//! which of two independent problems gets reported change from run to
//! run — exactly the class of defect this crate has already hit once.

use std::collections::{HashMap, HashSet, VecDeque};

use alba_syntax::Span;

use crate::error::CoreError;
use crate::eval::suggest;
use crate::model::{Beam, BeamId, Project};

/// Every beam id declared in `project`, in declaration order — the
/// candidate list offered to [`suggest`] for an "unknown beam" error's
/// "did you mean...?" help.
fn all_ids(project: &Project) -> impl Iterator<Item = &str> {
    project.beams.iter().map(|b| b.id.0.as_str())
}

/// Builds the "unknown beam `name`" error `validate_graph` and
/// `execution_subgraph` both need, with a "did you mean...?" suggestion
/// attached when a close-enough candidate exists. The caller stamps the
/// right `source_id` afterward (see [`CoreError::with_source_id`]) since
/// this helper has no beam of its own to read one from.
fn unknown_beam_error<'a>(
    name: &str,
    candidates: impl Iterator<Item = &'a str>,
    span: Span,
) -> CoreError {
    let err = CoreError::new(format!("unknown beam `{name}`"), span);
    match suggest(name, candidates) {
        Some(candidate) => err.with_help(format!("did you mean `{candidate}`?")),
        None => err,
    }
}

/// Checks that every `needs` entry in `project` names a beam that actually
/// exists, and that no beam (transitively) needs itself.
///
/// The two checks run as separate passes over [`Project::beams`], in
/// declaration order: first every `needs` entry against the full set of
/// declared ids, then cycle detection (which assumes every `needs` entry
/// already resolves, since the first pass would have already returned an
/// error otherwise). Both passes report against whichever beam declared
/// the offending `needs` entry — its own `span` and `source`, via
/// [`CoreError::with_source_id`] — never the caller's; this function runs
/// after every file has finished loading, when no `SourceIdScope` is
/// active, so leaving `source_id` at its default would always point a
/// multi-file project's error at the root file regardless of which
/// imported file actually declared the problem.
pub fn validate_graph(project: &Project) -> Result<(), CoreError> {
    let by_id: HashMap<&str, &Beam> = project.beams.iter().map(|b| (b.id.0.as_str(), b)).collect();

    for beam in &project.beams {
        for need in &beam.needs {
            if !by_id.contains_key(need.0.as_str()) {
                return Err(unknown_beam_error(&need.0, all_ids(project), beam.span)
                    .with_source_id(beam.source));
            }
        }
    }

    let mut done: HashSet<&str> = HashSet::new();
    for beam in &project.beams {
        if done.contains(beam.id.0.as_str()) {
            continue;
        }
        let mut path: Vec<&str> = Vec::new();
        detect_cycle(beam, &by_id, &mut done, &mut path)?;
    }

    Ok(())
}

/// Depth-first search from `beam`, following `needs` edges, tracking the
/// current DFS branch in `path` (ids "on the stack" for this walk, not the
/// whole project). `done` records beams whose subtree has already been
/// fully explored (by this call or an earlier one from
/// [`validate_graph`]'s outer loop), so a beam reachable from more than
/// one root is never walked twice.
///
/// A cycle is detected the instant `beam`'s own id is already present in
/// `path`: the reported cycle is `path[pos..]` (the loop itself, starting
/// from where it closes) with `beam`'s id appended once more to show it
/// closing — not the full DFS path from whatever root started this walk.
/// For `build → codegen → build`, `path` at the point of detection is
/// `["build", "codegen"]` and `beam` is `build` again, so `pos` is `0` and
/// the reported cycle is exactly `["build", "codegen", "build"]`. A
/// self-referencing beam (`a` needing `a`) falls out of the same logic:
/// `path` is `["a"]`, `beam` is `a`, `pos` is `0`, reported as `"a → a"`.
///
/// The error is stamped with the closing beam's own `span`/`source` (see
/// [`validate_graph`]'s doc comment) — the model doesn't carry a span for
/// an individual `needs` entry, only for the beam declaration as a whole,
/// so that's the most precise location available.
fn detect_cycle<'a>(
    beam: &'a Beam,
    by_id: &HashMap<&'a str, &'a Beam>,
    done: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
) -> Result<(), CoreError> {
    let id = beam.id.0.as_str();

    if let Some(pos) = path.iter().position(|&on_stack| on_stack == id) {
        let mut cycle: Vec<&str> = path[pos..].to_vec();
        cycle.push(id);
        return Err(CoreError::new(
            format!("dependency cycle: {}", cycle.join(" → ")),
            beam.span,
        )
        .with_source_id(beam.source));
    }

    if done.contains(id) {
        return Ok(());
    }

    path.push(id);
    for need in &beam.needs {
        // Safe to index directly: `validate_graph`'s first pass already
        // rejected any `needs` entry that doesn't resolve, and
        // `execution_subgraph` never calls this function.
        detect_cycle(by_id[need.0.as_str()], by_id, done, path)?;
    }
    path.pop();
    done.insert(id);

    Ok(())
}

/// The transitive closure of `target` (itself included), unordered:
/// scheduling order is the engine's job, not this crate's. A `needs`
/// entry repeated within one beam (`needs [a, a]`) contributes `a` to the
/// closure once, not twice.
///
/// An unknown `target` is an error, with the same "did you mean...?"
/// treatment [`validate_graph`] gives an unknown `needs` entry — but
/// stamped with the default `source_id` ([`crate::model::SourceId`]'s
/// `Default`, id `0`) rather than a beam's, since `target` is a plain
/// caller-supplied [`BeamId`] with no declaration site of its own to
/// attribute the error to.
///
/// Every `needs` entry this walks is assumed to resolve: any [`Project`]
/// a caller can actually hold came from [`crate::loader::load_project`] or
/// [`crate::eval::load_str`], both of which run [`validate_graph`] first —
/// so an unresolved `needs` entry here would mean that invariant was
/// somehow bypassed. Rather than panicking on that (a library should not
/// crash its caller over an invariant it cannot fully enforce, since
/// `Project`'s fields are public), a `needs` entry that doesn't resolve is
/// silently skipped instead of extended into.
pub fn execution_subgraph(project: &Project, target: &BeamId) -> Result<Vec<BeamId>, CoreError> {
    let by_id: HashMap<&str, &Beam> = project.beams.iter().map(|b| (b.id.0.as_str(), b)).collect();

    let Some(&start) = by_id.get(target.0.as_str()) else {
        return Err(unknown_beam_error(
            &target.0,
            all_ids(project),
            Span::new(0, 0),
        ));
    };

    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    seen.insert(start.id.0.as_str());
    queue.push_back(start.id.0.as_str());

    let mut closure = Vec::new();
    while let Some(id) = queue.pop_front() {
        closure.push(BeamId(id.to_string()));
        if let Some(&beam) = by_id.get(id) {
            for need in &beam.needs {
                if seen.insert(need.0.as_str()) {
                    queue.push_back(need.0.as_str());
                }
            }
        }
    }

    Ok(closure)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::eval::load_str;
    use crate::loader::load_project;
    use crate::model::SourceId;

    /// Sorts a closure's ids into plain, comparable `String`s — the brief
    /// is explicit that `execution_subgraph`'s result is unordered
    /// (scheduling order is the engine's job), so every test compares a
    /// sorted view rather than depending on traversal order.
    fn sorted(ids: Vec<BeamId>) -> Vec<String> {
        let mut v: Vec<String> = ids.into_iter().map(|id| id.0).collect();
        v.sort();
        v
    }

    /// Writes `content` to `path`, creating any missing parent directories
    /// first. Mirrors `tests/loader.rs`'s helper of the same name, for the
    /// one test here that needs a real multi-file project on disk (to
    /// check the `source_id` an imported file's own error carries).
    fn write(path: PathBuf, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn unknown_need_suggests_closest() {
        let err =
            load_str("beam build { needs [tset] run \"x\" }\nbeam test { run \"y\" }").unwrap_err();
        assert!(err.message.contains("unknown beam `tset`"));
        assert_eq!(err.help.as_deref(), Some("did you mean `test`?"));
    }

    #[test]
    fn cycle_reports_exact_path() {
        let err = load_str(
            "
beam build { needs [codegen] run \"a\" }
beam codegen { needs [build] run \"b\" }
",
        )
        .unwrap_err();
        assert!(err.message.contains("build → codegen → build"));
    }

    #[test]
    fn subgraph_is_transitive_closure() {
        let project = load_str(
            "
beam a { run \"x\" }
beam b { needs [a] run \"x\" }
beam c { needs [b] run \"x\" }
beam unrelated { run \"x\" }
",
        )
        .unwrap();
        let ids = execution_subgraph(&project, &BeamId("c".into())).unwrap();
        assert_eq!(sorted(ids), vec!["a", "b", "c"]);
    }

    /// Edge case called out explicitly: a beam needing itself must report
    /// `a → a`, not panic or loop forever.
    #[test]
    fn self_cycle_reports_itself() {
        let err = load_str("beam a { needs [a] run \"x\" }").unwrap_err();
        assert!(err.message.contains("a → a"));
    }

    /// `validate_graph` checks the whole project, not just whatever is
    /// reachable from some particular target — a cycle nobody asked to run
    /// is still a load error.
    #[test]
    fn cycle_unreachable_from_any_target_is_still_an_error() {
        let err = load_str(
            "
beam main { run \"x\" }
beam x { needs [y] run \"x\" }
beam y { needs [x] run \"x\" }
",
        )
        .unwrap_err();
        assert!(err.message.contains("x → y → x"));
    }

    /// A `needs` entry repeated within one beam is not an error, and does
    /// not duplicate the closure entry it names.
    #[test]
    fn duplicate_needs_do_not_duplicate_in_subgraph() {
        let project = load_str(
            "
beam a { run \"x\" }
beam b { needs [a, a] run \"x\" }
",
        )
        .unwrap();
        let ids = execution_subgraph(&project, &BeamId("b".into())).unwrap();
        assert_eq!(sorted(ids), vec!["a", "b"]);
    }

    #[test]
    fn unknown_target_suggests_closest() {
        let project = load_str("beam build { run \"x\" }").unwrap();
        let err = execution_subgraph(&project, &BeamId("biuld".into())).unwrap_err();
        assert!(err.message.contains("unknown beam `biuld`"));
        assert_eq!(err.help.as_deref(), Some("did you mean `build`?"));
    }

    /// Guards against the class of bug this crate has already hit once:
    /// which of two independent cycles gets reported must not depend on
    /// `HashMap` iteration order, which is randomized per instance (a
    /// fresh `by_id` map is built on every call). Run repeatedly in one
    /// process so a flaky dependency on that order would actually surface.
    #[test]
    fn cycle_report_is_deterministic_across_runs() {
        let src = "
beam a1 { needs [a2] run \"x\" }
beam a2 { needs [a1] run \"x\" }
beam b1 { needs [b2] run \"x\" }
beam b2 { needs [b1] run \"x\" }
";
        for _ in 0..20 {
            let err = load_str(src).unwrap_err();
            assert!(err.message.contains("a1 → a2 → a1"));
        }
    }

    /// The `source_id` half of the wiring: `validate_graph` runs after
    /// every file has finished loading, with no `SourceIdScope` active, so
    /// it must read the offending beam's own `source` field rather than
    /// defaulting to the root file's id. `api/Beamfile` here is registered
    /// second (`SourceId(1)`); a bug that left `source_id` at its default
    /// would report `SourceId(0)` (the root) instead.
    #[test]
    fn unknown_need_in_imported_file_reports_its_own_source_id() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path().join("Beamfile"),
            "import \"api/Beamfile\" as api\nbeam all { needs [api:build] run \"echo\" }",
        );
        write(
            dir.path().join("api/Beamfile"),
            "beam build { needs [tset] run \"x\" }\nbeam test { run \"y\" }",
        );

        let err = load_project(&dir.path().join("Beamfile")).unwrap_err();

        assert!(err.error.message.contains("unknown beam `api:tset`"));
        assert_eq!(err.error.help.as_deref(), Some("did you mean `api:test`?"));
        assert_eq!(err.error.source_id, SourceId(1));
    }
}
