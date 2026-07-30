//! The scheduler: turns a validated [`Project`] and a target beam into a
//! bounded-parallel run.
//!
//! ## Shape
//!
//! [`run`] extracts the target's execution subgraph, rejects the two
//! things that make a run unschedulable as a whole (an executor Alba does
//! not implement, and parameters that do not match what the target
//! declares) *before* starting any work, then spawns one tokio task per
//! beam. Each task waits for its dependencies, renders its `run`/`env`
//! templates with the target's parameters in scope, takes a permit from a
//! semaphore sized by [`RunOptions::jobs`], consults the cache, and — on a
//! miss — runs its commands sequentially, stopping at the first one that
//! fails. The permit covers the cache decision as well as the commands:
//! deciding reads every input file, which is work `jobs` must bound too.
//!
//! Anything that goes wrong from the moment a beam starts — a command that
//! exits non-zero, one that cannot be spawned, a template that will not
//! render — is that beam's own failure, reported through its normal event
//! stream and its status, never an abandoned run. That is what lets
//! `keep_going`, `allow_failure`, and dependency propagation govern it.
//!
//! ## Completion signalling
//!
//! Every beam owns a `watch` channel carrying `Option<BeamOutcome>`, and
//! its dependents hold receivers cloned *before* any task is spawned.
//! `watch::Receiver::wait_for` reads the channel's current value before it
//! ever suspends, so a beam that finishes before a dependent starts
//! waiting is still observed — a bare notification primitive would lose
//! that wakeup. Nothing in this module holds a lock (or a `watch` borrow
//! guard) across an `.await`.
//!
//! ## Two cancellation tokens
//!
//! The caller's token stops *running* commands: it is what reaches an
//! [`ExecContext`], so cancelling it terminates child processes. `stop` is
//! a child of it and means "no beam may start from now on": fail-fast
//! cancels `stop` only, which is precisely why a failure cancels the beams
//! that have not started while the ones already running finish. Because
//! `stop` is a child token, the caller's cancellation implies it too.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alba_core::{
    Beam, BeamId, CoreError, ExecutorKind, Project, execution_subgraph, expand_globs,
    outputs_satisfied, render_template,
};
use alba_executors::{CommandSpec, ExecContext, Executor, OutputLine, Stream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::EngineError;
use crate::cache::{
    BeamFacts, CacheOptions, CacheStore, FORMAT_VERSION, Manifest, fingerprint, hash_file,
    static_contribution,
};
use crate::event::{BeamStatus, RunEvent, RunSummary};

/// How a run is parameterized: everything the caller chose, as opposed to
/// what the Beamfile declares.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// How many beams may run at once. Zero is treated as one — a run with
    /// no slots at all could only ever hang.
    pub jobs: usize,
    /// Keep running everything still possible after a failure, instead of
    /// cancelling the beams that have not started.
    pub keep_going: bool,
    /// Positional arguments for the target beam, e.g. `staging` in
    /// `alba run deploy staging`.
    pub params: Vec<String>,
    /// Where the cache lives and how this run treats it. `None` disables
    /// caching entirely — `alba check` and most tests want that.
    pub cache: Option<CacheOptions>,
}

/// The executors a run can dispatch to, chosen per beam.
#[derive(Clone)]
pub struct Executors {
    /// `ExecutorKind::Shell`: the default (the embedded shell, once the
    /// CLI wires it in).
    pub embedded: Arc<dyn Executor>,
    /// `ExecutorKind::SystemShell`: the per-beam opt-out.
    pub system: Arc<dyn Executor>,
}

impl Executors {
    /// Both slots on the same executor: what tests and `alba check` want.
    pub fn uniform(executor: Arc<dyn Executor>) -> Self {
        Self {
            embedded: Arc::clone(&executor),
            system: executor,
        }
    }

    /// Which executor a beam's declared kind dispatches to.
    ///
    /// `Docker` never reaches here: [`plan`] rejects a docker beam during
    /// validation, before any beam task is built, so this beam's kind is
    /// always `Shell` or `SystemShell` by the time a task asks.
    fn for_beam(&self, kind: &ExecutorKind) -> Arc<dyn Executor> {
        match kind {
            ExecutorKind::Shell => Arc::clone(&self.embedded),
            ExecutorKind::SystemShell => Arc::clone(&self.system),
            ExecutorKind::Docker { .. } => {
                unreachable!("docker executor is rejected during validation, before scheduling")
            }
        }
    }
}

/// The fingerprint's own name for a beam's executor: participates in the
/// cache key (see [`crate::cache::BeamFacts::executor`]) so switching a
/// beam between `Shell` and `SystemShell` invalidates its cache entry even
/// when nothing else about it changed.
fn executor_label(kind: &ExecutorKind) -> &'static str {
    match kind {
        ExecutorKind::Shell => "embedded",
        ExecutorKind::SystemShell => "system",
        ExecutorKind::Docker { .. } => {
            unreachable!("docker executor is rejected during validation, before scheduling")
        }
    }
}

/// Runs `target` and everything it needs, and reports what happened.
///
/// Returns `Err` only for something Alba itself cannot do — an unknown
/// target, a docker beam, wrong parameters — or for a beam's task
/// panicking, which is a bug rather than an outcome. A beam that merely
/// fails is not an error: it lands in [`RunSummary::failed`], and
/// [`RunSummary::exit_code`] turns that into the process exit code.
/// `RunFinished` is emitted last, and only on the `Ok` path — an `Err` has
/// no summary to report.
pub async fn run(
    project: &Project,
    target: &BeamId,
    options: RunOptions,
    executors: Executors,
    events: UnboundedSender<RunEvent>,
    cancel: CancellationToken,
) -> Result<RunSummary, EngineError> {
    let started_at = Instant::now();
    let beams = plan(project, target, &options.params)?;

    let _ = events.send(RunEvent::RunStarted {
        target: target.clone(),
        beams: beams.iter().map(|beam| beam.id.clone()).collect(),
        edges: beams
            .iter()
            .flat_map(|beam| {
                beam.needs
                    .iter()
                    .map(|need| (beam.id.clone(), need.value.clone()))
            })
            .collect(),
    });

    let slots = Arc::new(Semaphore::new(options.jobs.max(1)));
    let stop = cancel.child_token();
    // One store for the whole run, shared by every task: it holds only a
    // path, and each entry is written atomically under the beam's own name.
    let cache = options
        .cache
        .as_ref()
        .map(|options| Arc::new(CacheStore::new(options.dir.clone())));
    let force = options.cache.as_ref().is_some_and(|options| options.force);

    // Both halves of every channel exist before the first task is spawned,
    // so no dependent can ever start waiting on a beam whose channel does
    // not exist yet. The senders are kept in a `Vec` parallel to `beams`
    // (rather than a map) so each task provably gets its own.
    let mut senders = Vec::with_capacity(beams.len());
    let mut receivers = HashMap::with_capacity(beams.len());
    for beam in &beams {
        let (sender, receiver) = watch::channel(None);
        senders.push(sender);
        receivers.insert(beam.id.0.as_str(), receiver);
    }

    let mut tasks = Vec::with_capacity(beams.len());
    for (beam, status) in beams.iter().zip(senders) {
        // Every `needs` entry resolves: the project passed graph
        // validation and `beams` is the target's transitive closure. The
        // assertion makes that an enforced invariant rather than a comment
        // — silently dropping an entry would let this beam run without
        // waiting for a dependency.
        let dependencies: Vec<_> = beam
            .needs
            .iter()
            .filter_map(|need| receivers.get(need.value.0.as_str()).cloned())
            .collect();
        debug_assert_eq!(
            dependencies.len(),
            beam.needs.len(),
            "every `needs` entry of `{}` must resolve within its own subgraph",
            beam.id.0
        );

        let task = BeamTask {
            args: if beam.id == *target {
                options.params.clone()
            } else {
                Vec::new()
            },
            // A spawned task must own its data; a `Beam` is a handful of
            // small vectors, cloned once per run.
            beam: (*beam).clone(),
            dependencies,
            status,
            slots: Arc::clone(&slots),
            executor: executors.for_beam(&beam.executor),
            events: events.clone(),
            cancel: cancel.clone(),
            stop: stop.clone(),
            keep_going: options.keep_going,
            cache: cache.clone(),
            force,
        };
        tasks.push((beam.id.clone(), tokio::spawn(run_beam(task))));
    }

    // Awaited in declaration order, which is also the order the summary
    // reports: a run's buckets must not depend on which task happened to
    // finish first. Every task is awaited even once one of them has
    // panicked, so no task outlives this call.
    let mut summary = RunSummary::default();
    let mut failure: Option<EngineError> = None;
    for (id, task) in tasks {
        match task.await {
            Ok(status) => summary.record(id, &status),
            Err(_) if failure.is_none() => failure = Some(EngineError::Panicked { beam: id }),
            Err(_) => {}
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }

    summary.duration = started_at.elapsed();
    let _ = events.send(RunEvent::RunFinished {
        summary: summary.clone(),
    });
    Ok(summary)
}

/// The beams to run, in the project's declaration order, once the two
/// things that make a run unschedulable as a whole have been ruled out.
///
/// Both checks happen here, before [`run`] spawns anything, so a docker
/// beam or a bad parameter list fails the run without half of its subgraph
/// having already executed. They are the only such checks: everything else
/// that can go wrong belongs to one beam and is reported as that beam's
/// failure once the run is under way.
fn plan<'a>(
    project: &'a Project,
    target: &BeamId,
    params: &[String],
) -> Result<Vec<&'a Beam>, EngineError> {
    let subgraph = execution_subgraph(project, target)?;
    let ids: HashSet<&str> = subgraph.iter().map(|id| id.0.as_str()).collect();
    let beams: Vec<&Beam> = project
        .beams
        .iter()
        .filter(|beam| ids.contains(beam.id.0.as_str()))
        .collect();

    if beams
        .iter()
        .any(|beam| matches!(beam.executor, ExecutorKind::Docker { .. }))
    {
        return Err(EngineError::Unschedulable(
            "docker executor is not yet supported".to_string(),
        ));
    }
    check_parameters(&beams, target, params)?;

    Ok(beams)
}

/// Checks that the run's positional arguments match what the target beam
/// declares, and that no other beam in the subgraph declares parameters —
/// only the target can be given any, so a dependency with parameters could
/// never have them bound.
fn check_parameters(
    beams: &[&Beam],
    target: &BeamId,
    params: &[String],
) -> Result<(), EngineError> {
    for beam in beams {
        if beam.id != *target && !beam.params.is_empty() {
            return Err(EngineError::Unschedulable(format!(
                "beam `{}` declares parameters, but only the target beam can be given any",
                beam.id.0
            )));
        }
    }

    // `execution_subgraph` already rejected a target that does not exist.
    let Some(beam) = beams.iter().find(|beam| beam.id == *target) else {
        return Ok(());
    };
    let expected = beam.params.len();
    if expected == params.len() {
        return Ok(());
    }

    let given = params.len();
    Err(EngineError::Unschedulable(if expected == 0 {
        format!("beam `{}` takes no parameters, got {given}", target.0)
    } else {
        let names = beam
            .params
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        let plural = if expected == 1 { "" } else { "s" };
        format!(
            "beam `{}` expects {expected} parameter{plural} ({names}), got {given}",
            target.0
        )
    }))
}

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

/// Everything one beam's task owns. A struct rather than a long argument
/// list, since all of it is handed to a spawned task as a unit.
struct BeamTask {
    beam: Beam,
    /// The target's positional arguments; empty for every other beam.
    args: Vec<String>,
    dependencies: Vec<watch::Receiver<Option<BeamOutcome>>>,
    status: watch::Sender<Option<BeamOutcome>>,
    slots: Arc<Semaphore>,
    executor: Arc<dyn Executor>,
    events: UnboundedSender<RunEvent>,
    /// The caller's token: reaches the executor, so cancelling it
    /// terminates commands that are already running.
    cancel: CancellationToken,
    /// "No beam may start from now on" — see the module doc comment.
    stop: CancellationToken,
    keep_going: bool,
    /// Where cache entries live; `None` when caching is disabled.
    cache: Option<Arc<CacheStore>>,
    /// Ignore any stored entry when reading, but still write one.
    force: bool,
}

/// One beam's whole life: wait, run (or not), report, release dependents.
///
/// Every way a beam can go wrong ends in a [`BeamStatus`], never in an
/// error that abandons the run: a command that exits non-zero, one that
/// cannot be spawned, and a template that cannot be rendered are all
/// per-beam outcomes, which is exactly what `keep_going` and
/// `allow_failure` govern. The only thing that can still take a beam's
/// task down is a panic, which [`run`] turns into an
/// [`EngineError::Panicked`].
///
/// Publishing the status is deliberately the last thing that happens, and
/// always happens: a dependent blocked on this beam is released only once
/// its `BeamFinished` event is out and, on a failure, only once `stop` has
/// been cancelled — so the dependent cannot observe a stale "nothing has
/// failed yet" and start.
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

/// Waits for every dependency and collects what each contributes to this
/// beam's fingerprint, in `needs` order. `None` means a dependency failed
/// or was cancelled — this beam must not run. `FailedAllowed` and
/// `Cached` both count as satisfied.
///
/// That a `FailedAllowed` dependency satisfies its dependents holds even
/// under `keep_going`: a beam whose dependency did not produce its outputs
/// cannot meaningfully run.
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

/// Blocks until `dependency` publishes its final outcome.
///
/// `wait_for` inspects the channel's current value before it suspends, so
/// a dependency that finished before this call is still observed. Its
/// borrow guard holds the channel's internal lock, so the outcome is cloned
/// out of it and the guard dropped before returning — never held across an
/// await.
async fn wait_for_outcome(dependency: &mut watch::Receiver<Option<BeamOutcome>>) -> BeamOutcome {
    let cancelled = BeamOutcome {
        status: BeamStatus::Cancelled,
        contribution: None,
    };
    match dependency.wait_for(Option::is_some).await {
        Ok(seen) => match &*seen {
            Some(outcome) => outcome.clone(),
            None => cancelled,
        },
        // The sender was dropped without publishing: that beam's task died
        // unexpectedly, so treat it as cancelled rather than wait forever.
        Err(_) => cancelled,
    }
}

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
///
/// The `jobs` slot is taken here, around both the assessment and the
/// commands, rather than around the commands alone. Assessing a beam
/// walks its tree and reads every input file whole; leaving that outside
/// the semaphore let every beam whose dependencies were satisfied do it at
/// once, which is the same resource problem `jobs` exists to bound.
async fn process(
    task: &BeamTask,
    contributions: &[Option<String>],
) -> (BeamStatus, Duration, Option<String>) {
    let cancelled = (BeamStatus::Cancelled, Duration::ZERO, None);

    // A run that is already stopping starts no beam, and holding a cache
    // hit is not an exception: this beam never started, so it is cancelled
    // like any other. `Cached` counts as satisfied, so replaying a hit here
    // would release this beam's dependents and let an aborting run report a
    // whole subgraph green. Checked before the assessment, not merely
    // before the hit is returned, so an aborting run also stops expanding
    // globs and hashing files instead of fingerprinting its way through the
    // rest of the graph.
    if task.stop.is_cancelled() {
        return cancelled;
    }

    let plan = render(&task.beam, &task.args).ok();
    let Some(permit) = acquire_slot(task).await else {
        return cancelled;
    };
    let (assessment, notice) = assess(task, plan.as_ref(), contributions).await;

    // Waiting for a slot, then hashing, may have taken a while, during
    // which the run may have been stopped: this beam has still not
    // started, and still holds no licence to report a hit.
    if task.stop.is_cancelled() {
        return cancelled;
    }

    if let Some(assessment) = &assessment
        && let Some(manifest) = &assessment.hit
    {
        // Released before replaying: a hit occupies a slot only for as
        // long as deciding it takes, never for reading its logs back.
        drop(permit);
        replay(task);
        return (
            BeamStatus::Cached,
            Duration::from_millis(manifest.duration_ms),
            Some(assessment.fingerprint.clone()),
        );
    }

    let contribution = match (&assessment, &plan) {
        (Some(assessment), _) => Some(assessment.fingerprint.clone()),
        (None, Some(plan)) => Some(static_contribution(
            &plan.commands,
            &plan.cwd.to_string_lossy(),
            &plan.env,
            &task.args,
            executor_label(&task.beam.executor),
        )),
        (None, None) => None,
    };

    let (status, duration, lines) = execute(task, plan, notice, permit).await;

    // Only a plain success is worth remembering: a failure, an allowed
    // failure, and a cancellation all leave the previous entry alone.
    if status == BeamStatus::Succeeded
        && let (Some(assessment), Some(store)) = (&assessment, &task.cache)
    {
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

    (status, duration, contribution)
}

/// The cache's verdict for this beam, plus an optional user-facing notice.
///
/// The verdict is `None` when the beam is not cacheable at all: caching
/// disabled, no declared `inputs`, `inputs` that resolve to no file at
/// all, a template that will not render, a dependency with no stable
/// contribution, or an input file that cannot be hashed. The last two
/// filesystem cases carry a notice — they are the ones worth telling the
/// user about, emitted as a stderr line once the beam starts.
async fn assess(
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

    let facts = CacheableBeam {
        store: Arc::clone(store),
        id: task.beam.id.clone(),
        dir: task.beam.dir.clone(),
        inputs: task.beam.inputs.clone(),
        outputs: task.beam.outputs.clone(),
        commands: plan.commands.clone(),
        cwd: plan.cwd.to_string_lossy().into_owned(),
        env: plan.env.clone(),
        args: task.args.clone(),
        needs,
        force: task.force,
        executor: executor_label(&task.beam.executor),
    };
    // Off the runtime's worker threads: `decide` walks a directory tree
    // and reads every input file whole through `std::fs`. Doing that in an
    // `async fn` blocked a worker for as long as it took, and with one
    // beam per worker doing it at once nothing else could be polled —
    // including the CLI's interrupt watcher and the task draining the
    // event channel, so Ctrl-C and the display both froze.
    //
    // A panic in there degrades like every other cache failure: the beam
    // runs uncached rather than taking the run down.
    tokio::task::spawn_blocking(move || facts.decide())
        .await
        .unwrap_or((None, None))
}

/// Everything the cache decision needs about one beam, owned rather than
/// borrowed: [`CacheableBeam::decide`] runs on a blocking thread, which
/// cannot hold a borrow of the scheduler's state.
struct CacheableBeam {
    store: Arc<CacheStore>,
    id: BeamId,
    dir: PathBuf,
    inputs: Vec<String>,
    outputs: Vec<String>,
    commands: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    args: Vec<String>,
    needs: Vec<String>,
    force: bool,
    executor: &'static str,
}

impl CacheableBeam {
    /// The blocking half of [`assess`]: resolve the inputs, hash them,
    /// fingerprint, and consult the store. Every step here touches the
    /// filesystem, which is exactly why it is kept in one place.
    fn decide(&self) -> (Option<Assessment>, Option<String>) {
        // A beam that declares `inputs` and resolves none of them is not
        // cacheable for this run. The fingerprint of an empty file list is
        // a constant, so the first run would write a manifest nothing can
        // ever invalidate and every later run would replay it — however
        // much the sources changed. The ordinary causes are all mistakes
        // worth naming: a misspelled path, an input directory the
        // project's `.gitignore` excludes, a pattern that will not compile.
        let matched = expand_globs(&self.dir, &self.inputs);
        if matched.is_empty() {
            return (
                None,
                Some("cache: `inputs` matched no file; running without cache".to_string()),
            );
        }

        let mut files = Vec::new();
        for (relative, absolute) in matched {
            match hash_file(&absolute) {
                Ok(hash) => files.push((relative, hash)),
                // Unreadable between glob resolution and hashing (deleted,
                // permissions): non-cacheable this run, never a failed run.
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
            commands: &self.commands,
            cwd: &self.cwd,
            env: &self.env,
            args: &self.args,
            needs: &self.needs,
            executor: self.executor,
        });

        let hit = (!self.force)
            .then(|| self.store.load(&self.id))
            .flatten()
            .filter(|manifest| manifest.fingerprint == fingerprint)
            .filter(|_| outputs_satisfied(&self.dir, &self.outputs));

        (Some(Assessment { fingerprint, hit }), None)
    }
}

/// Announces a hit, then replays its stored output lines in order —
/// `BeamCached` first, so a consumer sees the hit before any of the
/// original run's lines, mirroring a live beam's started/output order.
fn replay(task: &BeamTask) {
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

/// Takes a `jobs` slot, or `None` when the run stopped before one came
/// free.
///
/// `biased` so a run that is already stopping does not start one more beam
/// just because a permit happened to be free at the same instant. The
/// semaphore lives in [`run`] until every task has been awaited, so it is
/// never closed; treating a closed one as a cancellation keeps this total
/// without a panic.
async fn acquire_slot(task: &BeamTask) -> Option<OwnedSemaphorePermit> {
    tokio::select! {
        biased;
        () = task.stop.cancelled() => None,
        permit = Arc::clone(&task.slots).acquire_owned() => permit.ok(),
    }
}

/// Runs the beam's commands on the slot its caller already holds,
/// returning how it ended, how long it took, and the output lines worth
/// storing.
async fn execute(
    task: &BeamTask,
    plan: Option<RenderedBeam>,
    notice: Option<String>,
    // Bound (rather than dropped as `_`) so the slot stays held until this
    // function returns.
    _permit: OwnedSemaphorePermit,
) -> (BeamStatus, Duration, Vec<OutputLine>) {
    let started_at = Instant::now();
    let _ = task.events.send(RunEvent::BeamStarted {
        id: task.beam.id.clone(),
    });

    // Output is forwarded by a task of its own so lines reach the consumer
    // while the command is still running, rather than being buffered until
    // it exits. Joining it before returning is what guarantees every
    // `BeamOutput` precedes this beam's `BeamFinished`; it ends when the
    // last sender is dropped, which an `Executor` does by the time
    // `execute` returns (the trait's contract).
    let (lines, output) = tokio::sync::mpsc::unbounded_channel();
    let forwarder = tokio::spawn(forward_output(
        task.beam.id.clone(),
        output,
        task.events.clone(),
    ));

    // Why this beam is running uncached, told where the user is already
    // looking rather than out of band.
    if let Some(notice) = notice {
        let _ = lines.send(OutputLine {
            stream: Stream::Stderr,
            text: notice,
        });
    }

    // A template that will not render is this beam's failure, not the
    // run's — the same rule a command that cannot be spawned follows, and
    // for the same reason: `env(NAME)` with no default is deliberately
    // deferred to this moment, so an unset variable is an ordinary per-beam
    // outcome that `keep_going` and `allow_failure` are meant to govern.
    // Abandoning the whole run instead discarded the summary, dropped the
    // final event, and left this beam with no events at all.
    //
    // `plan` is what the caller already rendered successfully; re-rendering
    // here is how a beam whose template does not render reaches the failure
    // path with its error message intact.
    let status = match plan.map_or_else(|| render(&task.beam, &task.args), Ok) {
        Ok(plan) => run_commands(task, &plan, &lines).await,
        Err(error) => {
            let _ = lines.send(OutputLine {
                stream: Stream::Stderr,
                text: error.to_string(),
            });
            failure_status(task)
        }
    };
    drop(lines);
    let lines = forwarder.await.unwrap_or_default();

    (status, started_at.elapsed(), lines)
}

/// How a beam that failed before running a single command is recorded:
/// `NO_EXIT_CODE`, through its own `allow_failure` flag.
fn failure_status(task: &BeamTask) -> BeamStatus {
    if task.beam.allow_failure {
        BeamStatus::FailedAllowed {
            exit_code: NO_EXIT_CODE,
        }
    } else {
        BeamStatus::Failed {
            exit_code: NO_EXIT_CODE,
        }
    }
}

/// A beam's commands and the environment they run in, all rendered.
struct RenderedBeam {
    commands: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
}

/// Renders the beam's schedule-time templates with its parameters bound.
///
/// Every error is stamped with the beam's own [`alba_core::SourceId`]:
/// there is no active source scope outside `alba-core`'s loading, so an
/// unstamped error would point the CLI's caret at the root Beamfile
/// whatever file this beam actually came from.
fn render(beam: &Beam, args: &[String]) -> Result<RenderedBeam, EngineError> {
    let scope = beam.scope.with_params(&beam.params, args);
    let stamp = |error: CoreError| EngineError::Core(error.with_source_id(beam.source));

    let mut commands = Vec::with_capacity(beam.run.len());
    for template in &beam.run {
        commands.push(render_template(template, &scope).map_err(stamp)?);
    }
    let mut env = Vec::with_capacity(beam.env.len());
    for (name, template) in &beam.env {
        env.push((
            name.clone(),
            render_template(template, &scope).map_err(stamp)?,
        ));
    }

    let cwd = match &beam.cwd {
        // `join` with an absolute `cwd` yields that absolute path, so a
        // beam can opt out of its Beamfile's directory.
        Some(cwd) => beam.dir.join(cwd),
        None => beam.dir.clone(),
    };

    Ok(RenderedBeam { commands, env, cwd })
}

/// The exit code reported for a command that never produced one, because
/// the executor could not run it at all. Matches `SystemShellExecutor`'s
/// own fallback for a child with no discrete exit code.
const NO_EXIT_CODE: i32 = -1;

/// Runs the beam's commands one after another, stopping at the first
/// failure, and classifies the outcome.
///
/// A command that could not be spawned at all is that beam's failure, not
/// the run's: a missing shell or a `cwd` that does not exist is a per-beam
/// problem, which is exactly what `keep_going` and `allow_failure` are
/// about. The executor's message would otherwise be lost, so it is emitted
/// as a stderr line first — the CLI already renders those where the user
/// is looking.
async fn run_commands(
    task: &BeamTask,
    plan: &RenderedBeam,
    output: &UnboundedSender<OutputLine>,
) -> BeamStatus {
    for command in &plan.commands {
        let spec = CommandSpec {
            command: command.clone(),
            env: plan.env.clone(),
            cwd: plan.cwd.clone(),
        };
        let context = ExecContext {
            output: output.clone(),
            cancel: task.cancel.clone(),
        };

        let exit_code = match task.executor.execute(spec, context).await {
            Ok(result) if result.exit_code == 0 => continue,
            Ok(result) => result.exit_code,
            Err(error) => {
                let _ = output.send(OutputLine {
                    stream: Stream::Stderr,
                    text: error.to_string(),
                });
                NO_EXIT_CODE
            }
        };

        // A cancelled command still reports an exit code, and no code is
        // reserved to mean "was killed" (`-1` is a legitimate one on
        // windows), so the token — not the code — is what tells a
        // cancellation apart from a genuine failure.
        return if task.cancel.is_cancelled() {
            BeamStatus::Cancelled
        } else if task.beam.allow_failure {
            BeamStatus::FailedAllowed { exit_code }
        } else {
            BeamStatus::Failed { exit_code }
        };
    }
    BeamStatus::Succeeded
}

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
