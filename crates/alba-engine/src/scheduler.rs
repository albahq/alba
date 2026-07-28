//! The scheduler: turns a validated [`Project`] and a target beam into a
//! bounded-parallel run.
//!
//! ## Shape
//!
//! [`run`] extracts the target's execution subgraph, rejects anything that
//! makes the run unschedulable *before* starting any work, then spawns one
//! tokio task per beam. Each task waits for its dependencies, takes a
//! permit from a semaphore sized by [`RunOptions::jobs`], renders its
//! `run`/`env` templates with the target's parameters in scope, and runs
//! its commands sequentially, stopping at the first one that fails.
//!
//! ## Completion signalling
//!
//! Every beam owns a `watch` channel carrying `Option<BeamStatus>`, and
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
    Beam, BeamId, CoreError, ExecutorKind, Project, execution_subgraph, render_template,
};
use alba_executors::{CommandSpec, ExecContext, Executor, OutputLine};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

use crate::EngineError;
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
}

/// Runs `target` and everything it needs, and reports what happened.
///
/// Returns `Err` only for something Alba itself cannot do — an unknown
/// target, a docker beam, wrong parameters, a template that fails to
/// render, an executor that could not run a command at all. A beam whose
/// command merely exits non-zero is not an error: it lands in
/// [`RunSummary::failed`], and [`RunSummary::exit_code`] turns that into
/// the process exit code. `RunFinished` is emitted last, and only on the
/// `Ok` path — an `Err` has no summary to report.
pub async fn run(
    project: &Project,
    target: &BeamId,
    options: RunOptions,
    executor: Arc<dyn Executor>,
    events: UnboundedSender<RunEvent>,
    cancel: CancellationToken,
) -> Result<RunSummary, EngineError> {
    let started_at = Instant::now();
    let beams = plan(project, target, &options.params)?;

    let slots = Arc::new(Semaphore::new(options.jobs.max(1)));
    let stop = cancel.child_token();

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
        // validation and `beams` is the target's transitive closure.
        let dependencies = beam
            .needs
            .iter()
            .filter_map(|need| receivers.get(need.0.as_str()).cloned())
            .collect();

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
            executor: Arc::clone(&executor),
            events: events.clone(),
            cancel: cancel.clone(),
            stop: stop.clone(),
            keep_going: options.keep_going,
        };
        tasks.push((beam.id.clone(), tokio::spawn(run_beam(task))));
    }

    // Awaited in declaration order, which is also the order the summary
    // reports: a run's buckets must not depend on which task happened to
    // finish first. Every task is awaited even once one of them has
    // errored, so no task outlives this call.
    let mut summary = RunSummary::default();
    let mut failure: Option<EngineError> = None;
    for (id, task) in tasks {
        let error = match task.await {
            Ok(Ok(status)) => {
                summary.record(id, &status);
                continue;
            }
            Ok(Err(error)) => error,
            Err(_) => EngineError::Panicked { beam: id },
        };
        if failure.is_none() {
            failure = Some(error);
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

/// The beams to run, in the project's declaration order, once everything
/// that makes the run unschedulable has been ruled out.
///
/// Both checks happen here, before [`run`] spawns anything, so a docker
/// beam or a bad parameter list fails the run without half of its subgraph
/// having already executed.
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

/// Everything one beam's task owns. A struct rather than a long argument
/// list, since all of it is handed to a spawned task as a unit.
struct BeamTask {
    beam: Beam,
    /// The target's positional arguments; empty for every other beam.
    args: Vec<String>,
    dependencies: Vec<watch::Receiver<Option<BeamStatus>>>,
    status: watch::Sender<Option<BeamStatus>>,
    slots: Arc<Semaphore>,
    executor: Arc<dyn Executor>,
    events: UnboundedSender<RunEvent>,
    /// The caller's token: reaches the executor, so cancelling it
    /// terminates commands that are already running.
    cancel: CancellationToken,
    /// "No beam may start from now on" — see the module doc comment.
    stop: CancellationToken,
    keep_going: bool,
}

/// One beam's whole life: wait, run (or not), report, release dependents.
///
/// Publishing the status is deliberately the last thing that happens, and
/// always happens: a dependent blocked on this beam is released only once
/// its `BeamFinished` event is out and, on a failure, only once `stop` has
/// been cancelled — so the dependent cannot observe a stale "nothing has
/// failed yet" and start.
async fn run_beam(mut task: BeamTask) -> Result<BeamStatus, EngineError> {
    let outcome = if dependencies_satisfied(&mut task.dependencies).await {
        execute(&task).await
    } else {
        Ok((BeamStatus::Cancelled, Duration::ZERO))
    };

    match outcome {
        Ok((status, duration)) => {
            if matches!(status, BeamStatus::Failed { .. }) && !task.keep_going {
                task.stop.cancel();
            }
            let _ = task.events.send(RunEvent::BeamFinished {
                id: task.beam.id.clone(),
                status: status.clone(),
                duration,
            });
            let _ = task.status.send(Some(status.clone()));
            Ok(status)
        }
        Err(error) => {
            // The run is being abandoned: stop everything that has not
            // started, and release the dependents waiting on this beam so
            // every task finishes and `run` can return the error.
            task.stop.cancel();
            let _ = task.status.send(Some(BeamStatus::Cancelled));
            Err(error)
        }
    }
}

/// Whether every dependency ended in a state that lets this beam run.
/// `FailedAllowed` counts as satisfied; `Failed` and `Cancelled` do not,
/// and that holds even under `keep_going` — a beam whose dependency did
/// not produce its outputs cannot meaningfully run.
async fn dependencies_satisfied(dependencies: &mut [watch::Receiver<Option<BeamStatus>>]) -> bool {
    for dependency in dependencies {
        match wait_for_status(dependency).await {
            BeamStatus::Succeeded | BeamStatus::FailedAllowed { .. } => {}
            BeamStatus::Failed { .. } | BeamStatus::Cancelled => return false,
        }
    }
    true
}

/// Blocks until `dependency` publishes its final status.
///
/// `wait_for` inspects the channel's current value before it suspends, so
/// a dependency that finished before this call is still observed. Its
/// borrow guard holds the channel's internal lock, so the status is cloned
/// out of it and the guard dropped before returning — never held across an
/// await.
async fn wait_for_status(dependency: &mut watch::Receiver<Option<BeamStatus>>) -> BeamStatus {
    match dependency.wait_for(Option::is_some).await {
        Ok(seen) => match &*seen {
            Some(status) => status.clone(),
            None => BeamStatus::Cancelled,
        },
        // The sender was dropped without publishing: that beam's task died
        // unexpectedly, so treat it as cancelled rather than wait forever.
        Err(_) => BeamStatus::Cancelled,
    }
}

/// Takes a slot and runs the beam's commands, returning how it ended and
/// how long it took. Returns `Cancelled` with a zero duration for a beam
/// that never got to start.
async fn execute(task: &BeamTask) -> Result<(BeamStatus, Duration), EngineError> {
    let cancelled = (BeamStatus::Cancelled, Duration::ZERO);

    // `biased` so a run that is already stopping does not start one more
    // beam just because a permit happened to be free at the same instant.
    let permit = tokio::select! {
        biased;
        () = task.stop.cancelled() => return Ok(cancelled),
        permit = task.slots.acquire() => permit,
    };
    // Bound (rather than dropped as `_`) so the slot stays held until this
    // function returns. The semaphore lives in `run` until every task has
    // been awaited, so it is never closed; treating a closed one as a
    // cancellation keeps this total without a panic.
    let Ok(_permit) = permit else {
        return Ok(cancelled);
    };
    // Waiting for the permit may have taken a while, during which the run
    // may have been stopped: this beam has still not started.
    if task.stop.is_cancelled() {
        return Ok(cancelled);
    }

    let plan = render(&task.beam, &task.args)?;

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

    let outcome = run_commands(task, &plan, &lines).await;
    drop(lines);
    let _ = forwarder.await;

    Ok((outcome?, started_at.elapsed()))
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

/// Runs the beam's commands one after another, stopping at the first
/// failure, and classifies the outcome.
async fn run_commands(
    task: &BeamTask,
    plan: &RenderedBeam,
    output: &UnboundedSender<OutputLine>,
) -> Result<BeamStatus, EngineError> {
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
        let result = task
            .executor
            .execute(spec, context)
            .await
            .map_err(|source| EngineError::Executor {
                beam: task.beam.id.clone(),
                source,
            })?;

        if result.exit_code == 0 {
            continue;
        }
        // A cancelled command still reports an exit code, and no code is
        // reserved to mean "was killed" (`-1` is a legitimate one on
        // windows), so the token — not the code — is what tells a
        // cancellation apart from a genuine failure.
        return Ok(if task.cancel.is_cancelled() {
            BeamStatus::Cancelled
        } else if task.beam.allow_failure {
            BeamStatus::FailedAllowed {
                exit_code: result.exit_code,
            }
        } else {
            BeamStatus::Failed {
                exit_code: result.exit_code,
            }
        });
    }
    Ok(BeamStatus::Succeeded)
}

/// Relabels an executor's output lines as this beam's output events, until
/// the executor drops the last sender.
async fn forward_output(
    id: BeamId,
    mut output: UnboundedReceiver<OutputLine>,
    events: UnboundedSender<RunEvent>,
) {
    while let Some(line) = output.recv().await {
        let _ = events.send(RunEvent::BeamOutput {
            id: id.clone(),
            line,
        });
    }
}
