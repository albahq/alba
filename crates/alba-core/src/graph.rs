//! Dependency-graph validation and subgraph extraction, the last piece a
//! [`Project`] needs before an engine can schedule it: [`validate_graph`]
//! checks that every `needs` entry — and the file's `default`, if it
//! declares one — names a beam that actually exists, and that no beam
//! (transitively) needs itself; [`execution_subgraph`] extracts the
//! transitive closure of a target beam.
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

/// An O(1) id-to-beam lookup for `project`, built fresh on every call.
/// Shared by [`validate_graph`] and [`execution_subgraph`], which both
/// need one, so the same map isn't built twice per call in a caller that
/// validates and then immediately extracts a subgraph. Only ever used for
/// lookups, never iterated — see the module doc comment's "Determinism"
/// section for why that distinction matters here.
fn by_id(project: &Project) -> HashMap<&str, &Beam> {
    project.beams.iter().map(|b| (b.id.0.as_str(), b)).collect()
}

/// Builds the "unknown beam `name`" error `validate_graph` and
/// `execution_subgraph` both need, with a "did you mean...?" suggestion
/// attached when a close-enough candidate exists. `span` is `None` when
/// the name came from outside the Beamfile and so has no declaration site
/// to underline. The caller stamps the right `source_id` afterward (see
/// [`CoreError::with_source_id`]) since this helper has no beam of its own
/// to read one from.
fn unknown_beam_error<'a>(
    name: &str,
    candidates: impl Iterator<Item = &'a str>,
    span: Option<Span>,
) -> CoreError {
    let message = format!("unknown beam `{name}`");
    let err = match span {
        Some(span) => CoreError::new(message, span),
        None => CoreError::unlocated(message),
    };
    match suggest(name, candidates) {
        Some(candidate) => err.with_help(format!("did you mean `{candidate}`?")),
        None => err,
    }
}

/// Checks that every `needs` entry in `project` names a beam that actually
/// exists, that `project`'s `default` (if it declares one) does too, and
/// that no beam (transitively) needs itself.
///
/// The checks run as separate passes over [`Project::beams`], in
/// declaration order: first every `needs` entry against the full set of
/// declared ids, then the `default`, then cycle detection (which assumes
/// every `needs` entry already resolves, since the first pass would have
/// already returned an error otherwise). Each reports at the exact
/// reference that does not resolve, and against whichever beam declared
/// it — its own `source`, via [`CoreError::with_source_id`] — never the
/// caller's; this function runs after every file has finished loading,
/// when no `SourceIdScope` is active, so leaving `source_id` at its
/// default would always point a multi-file project's error at the root
/// file regardless of which imported file actually declared the problem.
/// The `default` needs no such correction: only the root file's `default`
/// is ever kept (see [`crate::loader::load_project`]), so it always
/// belongs to the file `SourceId`'s own default already names.
pub fn validate_graph(project: &Project) -> Result<(), CoreError> {
    let by_id = by_id(project);

    for beam in &project.beams {
        for need in &beam.needs {
            if !by_id.contains_key(need.value.0.as_str()) {
                return Err(
                    unknown_beam_error(&need.value.0, all_ids(project), Some(need.span))
                        .with_source_id(beam.source),
                );
            }
        }
    }

    if let Some(default) = &project.default
        && !by_id.contains_key(default.value.0.as_str())
    {
        return Err(unknown_beam_error(
            &default.value.0,
            all_ids(project),
            Some(default.span),
        ));
    }

    let mut done: HashSet<&str> = HashSet::new();
    for beam in &project.beams {
        if done.contains(beam.id.0.as_str()) {
            continue;
        }
        detect_cycle(beam, &by_id, &mut done)?;
    }

    Ok(())
}

/// One entry of [`detect_cycle`]'s explicit search stack: either a beam to
/// descend into, or the marker that pops a beam back off the current path
/// once its whole subtree has been explored (what returning from a
/// recursive call would have done).
enum Step<'a> {
    Enter(&'a Beam),
    Leave(&'a str),
}

/// Depth-first search from `root`, following `needs` edges, tracking the
/// current branch in `path` (ids "on the stack" for this walk, not the
/// whole project). `done` records beams whose subtree has already been
/// fully explored (by this call or an earlier one from
/// [`validate_graph`]'s outer loop), so a beam reachable from more than
/// one root is never walked twice.
///
/// The search keeps its own heap-allocated stack rather than recursing per
/// `needs` edge. Depth here is a property of the *Beamfile*, not of Alba: a
/// chain of beams each needing the next, declared in that order, descends
/// as deep as the chain is long. Recursion made that a stack overflow —
/// which aborts the process outright rather than panicking, so a library
/// would take its caller down with no diagnostic at all.
///
/// A cycle is detected the instant a beam's own id is already present in
/// `path`: the reported cycle is `path[pos..]` (the loop itself, starting
/// from where it closes) with that id appended once more to show it
/// closing — not the full path from whatever root started this walk. For
/// `build → codegen → build`, `path` at the point of detection is
/// `["build", "codegen"]` and the beam being entered is `build` again, so
/// `pos` is `0` and the reported cycle is exactly
/// `["build", "codegen", "build"]`. A self-referencing beam (`a` needing
/// `a`) falls out of the same logic: `path` is `["a"]`, the beam is `a`,
/// `pos` is `0`, reported as `"a → a"`.
///
/// Visiting order matches what recursion produced, so which of several
/// independent cycles gets reported does not change: a beam's `needs` are
/// pushed in reverse so the first entry is the first one entered.
///
/// The error is stamped with the closing beam's own `span`/`source` (see
/// [`validate_graph`]'s doc comment).
fn detect_cycle<'a>(
    root: &'a Beam,
    by_id: &HashMap<&'a str, &'a Beam>,
    done: &mut HashSet<&'a str>,
) -> Result<(), CoreError> {
    let mut path: Vec<&'a str> = Vec::new();
    let mut stack: Vec<Step<'a>> = vec![Step::Enter(root)];

    while let Some(step) = stack.pop() {
        let beam = match step {
            Step::Leave(id) => {
                path.pop();
                done.insert(id);
                continue;
            }
            Step::Enter(beam) => beam,
        };
        let id = beam.id.0.as_str();

        if let Some(pos) = path.iter().position(|&on_path| on_path == id) {
            let mut cycle: Vec<&str> = path[pos..].to_vec();
            cycle.push(id);
            return Err(CoreError::new(
                format!("dependency cycle: {}", cycle.join(" → ")),
                beam.span,
            )
            .with_source_id(beam.source));
        }

        if done.contains(id) {
            continue;
        }

        path.push(id);
        stack.push(Step::Leave(id));
        for need in beam.needs.iter().rev() {
            // Safe to index directly: `validate_graph`'s first pass already
            // rejected any `needs` entry that doesn't resolve, and
            // `execution_subgraph` never calls this function.
            stack.push(Step::Enter(by_id[need.value.0.as_str()]));
        }
    }

    Ok(())
}

/// The transitive closure of `target` (itself included), unordered:
/// scheduling order is the engine's job, not this crate's. A `needs`
/// entry repeated within one beam (`needs [a, a]`) contributes `a` to the
/// closure once, not twice.
///
/// An unknown `target` is an error, with the same "did you mean...?"
/// treatment [`validate_graph`] gives an unknown `needs` entry — but with
/// no span at all, since `target` is a plain caller-supplied [`BeamId`]
/// (typically typed on a command line) with no declaration site of its own
/// to point at. Underlining an arbitrary beam declaration instead would
/// assert a source position that is not where the mistake is.
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
    let by_id = by_id(project);

    let Some(&start) = by_id.get(target.0.as_str()) else {
        return Err(unknown_beam_error(&target.0, all_ids(project), None));
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
                if seen.insert(need.value.0.as_str()) {
                    queue.push_back(need.value.0.as_str());
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

    /// Sorts a closure's ids into plain, comparable `String`s.
    /// `execution_subgraph`'s result is unordered by contract (scheduling
    /// order is the engine's job), so every test compares a sorted view
    /// rather than depending on traversal order.
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

    /// The caret lands on the entry that does not resolve, not on the beam
    /// declaration that happens to contain it — every other diagnostic in
    /// this workspace is token-precise, and a `needs` entry is a token.
    #[test]
    fn unknown_need_points_at_the_offending_entry() {
        const SOURCE: &str = "beam build { needs [test, tset] run \"x\" }\nbeam test { run \"y\" }";

        let err = load_str(SOURCE).unwrap_err();

        let span = err.span.expect("a declared `needs` entry has a location");
        assert_eq!(&SOURCE[span.start..span.end], "tset");
    }

    /// A namespaced entry is underlined whole, alias included.
    #[test]
    fn unknown_namespaced_need_points_at_the_whole_reference() {
        const SOURCE: &str = "beam build { needs [api:tset] run \"x\" }";

        let err = load_str(SOURCE).unwrap_err();

        let span = err.span.expect("a declared `needs` entry has a location");
        assert_eq!(&SOURCE[span.start..span.end], "api:tset");
    }

    /// `default` is part of the graph too: naming a beam that does not
    /// exist must fail the load, not wait until a bare `alba` tries to run
    /// it.
    #[test]
    fn unknown_default_is_rejected_at_its_own_span() {
        const SOURCE: &str = "default biuld\nbeam build { run \"x\" }";

        let err = load_str(SOURCE).unwrap_err();

        assert!(err.message.contains("unknown beam `biuld`"));
        assert_eq!(err.help.as_deref(), Some("did you mean `build`?"));
        let span = err.span.expect("a declared `default` has a location");
        assert_eq!(&SOURCE[span.start..span.end], "biuld");
    }

    /// A `default` that resolves is not disturbed by the check.
    #[test]
    fn a_default_naming_a_declared_beam_loads() {
        let project = load_str("default build\nbeam build { run \"x\" }").unwrap();

        assert_eq!(
            project.default.as_ref().map(|d| d.value.0.as_str()),
            Some("build")
        );
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

    /// A target is supplied by whoever asked for the run, not written in
    /// the Beamfile, so it has no location to underline — drawing a caret
    /// at the first beam declaration asserted a source position that is
    /// simply not where the mistake is. The "did you mean...?" help, which
    /// is the useful half, survives.
    #[test]
    fn unknown_target_suggests_closest_without_a_source_location() {
        let project = load_str("beam build { run \"x\" }").unwrap();
        let err = execution_subgraph(&project, &BeamId("biuld".into())).unwrap_err();
        assert!(err.message.contains("unknown beam `biuld`"));
        assert_eq!(err.help.as_deref(), Some("did you mean `build`?"));
        assert!(
            err.span.is_none(),
            "a target named outside the Beamfile has no span"
        );
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

    /// Distinguishes correct cycle-path slicing (`path[pos..]`) from the
    /// bug it guards against (reporting the whole DFS path from whatever
    /// root started the walk, `path[0..]`): a long, non-cyclic prefix
    /// leads into the cycle, and none of that prefix's ids may appear in
    /// the message — only the loop itself. `cycle_reports_exact_path`
    /// alone doesn't distinguish these, because its DFS root is already
    /// part of the cycle.
    #[test]
    fn cycle_message_excludes_the_non_cyclic_dfs_prefix() {
        let err = load_str(
            "
beam root { needs [mid] run \"x\" }
beam mid { needs [inner] run \"x\" }
beam inner { needs [x] run \"x\" }
beam x { needs [y] run \"x\" }
beam y { needs [x] run \"x\" }
",
        )
        .unwrap_err();
        assert!(err.message.contains("x → y → x"));
        assert!(!err.message.contains("root"));
        assert!(!err.message.contains("mid"));
        assert!(!err.message.contains("inner"));
    }

    /// `d` needs both `b` and `c`, which both need `a`: `a` must appear in
    /// the closure exactly once, distinguishing the `seen`-guarded BFS
    /// from one that pushes every `needs` entry unconditionally.
    /// `duplicate_needs_do_not_duplicate_in_subgraph` only covers the easy
    /// half (the same id repeated within one beam's own `needs` list) —
    /// this covers two different beams sharing a dependency.
    #[test]
    fn diamond_shaped_needs_deduplicates_shared_dependency() {
        let project = load_str(
            "
beam a { run \"x\" }
beam b { needs [a] run \"x\" }
beam c { needs [a] run \"x\" }
beam d { needs [b, c] run \"x\" }
",
        )
        .unwrap();
        let ids = execution_subgraph(&project, &BeamId("d".into())).unwrap();
        assert_eq!(ids.iter().filter(|id| id.0 == "a").count(), 1);
        assert_eq!(sorted(ids), vec!["a", "b", "c", "d"]);
    }

    /// A five-level chain, to confirm the BFS actually walks multiple hops
    /// to completion rather than stopping early — `subgraph_is_transitive_
    /// closure`'s three levels leave that under-exercised.
    #[test]
    fn subgraph_walks_a_deep_chain_completely() {
        let project = load_str(
            "
beam a { run \"x\" }
beam b { needs [a] run \"x\" }
beam c { needs [b] run \"x\" }
beam d { needs [c] run \"x\" }
beam e { needs [d] run \"x\" }
",
        )
        .unwrap();
        let ids = execution_subgraph(&project, &BeamId("e".into())).unwrap();
        assert_eq!(sorted(ids), vec!["a", "b", "c", "d", "e"]);
    }

    /// A chain declared so that every beam needs the one declared *after*
    /// it: nothing is marked done ahead of the walk, so cycle detection
    /// descends the full depth in one go. A recursive walk aborts the
    /// process here — a stack overflow is not a panic, so no caller can
    /// catch it — which is why the search keeps its own heap-allocated
    /// stack.
    #[test]
    fn cycle_detection_survives_a_very_deep_chain() {
        const DEPTH: usize = 10_000;

        let mut source = String::new();
        for index in 0..DEPTH - 1 {
            source.push_str(&format!(
                "beam b{index} {{ needs [b{}] run \"x\" }}\n",
                index + 1
            ));
        }
        source.push_str(&format!("beam b{} {{ run \"x\" }}\n", DEPTH - 1));

        let project = load_str(&source).unwrap();

        assert_eq!(project.beams.len(), DEPTH);
    }

    /// The same depth, closed into a cycle by its last beam: the reported
    /// path must still be exact rather than truncated or reordered by the
    /// iterative walk.
    #[test]
    fn a_cycle_at_the_bottom_of_a_deep_chain_is_reported_exactly() {
        const DEPTH: usize = 5_000;

        let mut source = String::new();
        for index in 0..DEPTH - 1 {
            source.push_str(&format!(
                "beam b{index} {{ needs [b{}] run \"x\" }}\n",
                index + 1
            ));
        }
        source.push_str(&format!(
            "beam b{} {{ needs [b{}] run \"x\" }}\n",
            DEPTH - 1,
            DEPTH - 2
        ));

        let err = load_str(&source).unwrap_err();

        assert!(
            err.message
                .contains(&format!("b{} → b{} → b{}", DEPTH - 2, DEPTH - 1, DEPTH - 2))
        );
        assert!(!err.message.contains("b0 "));
    }

    /// A cycle entirely among *imported*, namespaced beams. `detect_cycle` compares `BeamId.0` as
    /// a plain string, so a `:`-namespaced id is no different from an
    /// unnamespaced one by inspection — this pins that down with a real
    /// multi-file fixture instead of leaving it unverified. `api/Beamfile`
    /// is registered second (`SourceId(1)`), so the error must carry that
    /// id, not the root's.
    #[test]
    fn cycle_among_imported_namespaced_beams_reports_qualified_path() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path().join("Beamfile"),
            "import \"api/Beamfile\" as api\nbeam all { needs [api:x] run \"echo\" }",
        );
        write(
            dir.path().join("api/Beamfile"),
            "beam x { needs [y] run \"x\" }\nbeam y { needs [x] run \"y\" }",
        );

        let err = load_project(&dir.path().join("Beamfile")).unwrap_err();

        assert!(err.error.message.contains("api:x → api:y → api:x"));
        assert_eq!(err.error.source_id, SourceId(1));
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
