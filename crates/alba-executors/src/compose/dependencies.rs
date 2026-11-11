//! The dependencies of a Compose service: brought up before the service's
//! own container, recorded with the state each one had before when Alba
//! started it, and put back in that state when the executor shuts down. The
//! pure bookkeeping lives in [`super::started`].

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::process::Command;

use crate::container::spawn_error;
use crate::{BeamContext, ExecError, OutputLine, Stream};

use super::service::{Condition, Dependency, ServiceDescription};
use super::started::{ProjectRecord, running_by_service, to_restore};
use super::{
    COMPOSE_REQUIREMENT, CREATION_SETTLE, Project, StepOutcome, run_step, start_cancelled,
    step_failed, step_stdout,
};

/// The dependencies a compose executor started, per Compose project name.
/// Each project's record is behind its own lock, held from reading the
/// dependencies' states to the end of their start, so that two sessions
/// opening at once cannot both believe they started the same service.
#[derive(Debug, Default)]
pub(super) struct Dependencies {
    records: Mutex<HashMap<String, Arc<tokio::sync::Mutex<ProjectRecord>>>>,
}

impl Dependencies {
    /// The record of the Compose project `name`, created empty, addressed
    /// through `project`, the first time a session of it needs one.
    fn record_of(&self, name: &str, project: &Project) -> Arc<tokio::sync::Mutex<ProjectRecord>> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(ProjectRecord {
                    dir: project.dir.clone(),
                    file: project.file.clone(),
                    started: Default::default(),
                    interrupted: false,
                }))
            })
            .clone()
    }

    /// Brings up the dependencies of the beam's service, recording first,
    /// under the project's lock, those that were absent or stopped: a start
    /// that fails or is cancelled halfway is still put back at shutdown.
    ///
    /// One `up` starts every required direct dependency, so that a one-shot
    /// several of them reach runs once: `up` runs an exited one-shot again
    /// each time it reaches it. Each optional one (`required: false`) gets
    /// its own `up`, and one that does not reach its condition writes one
    /// warning line on the beam's output instead of failing the start; a
    /// one-shot that an optional dependency and another dependency both
    /// reach runs once per `up`. An optional dependency that is not a direct
    /// one follows Compose's own `up`, which fails when it cannot start.
    ///
    /// `up` honours the conditions of the transitive dependencies itself;
    /// the direct ones are then checked, since `up` names them rather than
    /// depending on them:
    ///
    /// - `service_started` needs nothing more;
    /// - `service_healthy` is awaited with `up --wait --no-deps`, which waits
    ///   for the named services only and reaches none of their dependencies;
    /// - `service_completed_successfully` is awaited with `docker wait` on
    ///   the one-shot's containers, which reports an exit code whether the
    ///   container is still running or already exited. `up --wait` cannot
    ///   serve: it fails on a one-shot that already exited, even with 0, and
    ///   returns while a slower one still runs. Nor can `docker compose
    ///   wait`: before Compose 5.6 it only sees running containers, and
    ///   fails on a one-shot that already exited.
    pub async fn start(
        &self,
        project: &Project,
        description: &ServiceDescription,
        beam: &BeamContext,
    ) -> Result<(), ExecError> {
        let service = description.service.as_str();
        let record = self.record_of(&description.project, project);
        let mut record = tokio::select! {
            record = record.lock() => record,
            _ = beam.cancel.cancelled() => return Err(start_cancelled()),
        };

        // Named, so that the read covers exactly the dependencies, whatever
        // profiles they sit behind.
        let mut ps_args = vec!["ps", "-a", "--format", "json"];
        ps_args.extend(description.dependencies.iter().map(String::as_str));
        let ps = run_step(project.command(&ps_args), beam).await?;
        if !ps.status.success() {
            return Err(step_failed("ps", ps.status, service));
        }
        let states = running_by_service(&step_stdout(ps, "ps")?)?;
        record.record(to_restore(&description.dependencies, &states));

        let (required, optional): (Vec<&Dependency>, Vec<&Dependency>) = description
            .direct_dependencies
            .iter()
            .partition(|dependency| dependency.required);

        if let Some(code) = up(project, beam, &mut record, &[], &names(&required, None)).await? {
            return Err(up_failed(code, service));
        }
        // Each optional dependency on its own `up`, so that one failing
        // keeps none of the others from starting.
        let mut optional_started = Vec::new();
        for dependency in optional {
            let name = [dependency.service.as_str()];
            match up(project, beam, &mut record, &[], &name).await? {
                None => optional_started.push(dependency),
                Some(code) => {
                    warn_skipped(beam, dependency, service, "did not start", &up_exited(code))
                }
            }
        }

        let healthy = Some(Condition::Healthy);
        let waited = ["--wait", "--no-deps"];
        if let Some(code) = up(
            project,
            beam,
            &mut record,
            &waited,
            &names(&required, healthy),
        )
        .await?
        {
            return Err(up_failed(code, service));
        }
        for dependency in &optional_started {
            let name = names(&[dependency], healthy);
            if let Some(code) = up(project, beam, &mut record, &waited, &name).await? {
                warn_skipped(
                    beam,
                    dependency,
                    service,
                    "did not become healthy",
                    &up_exited(code),
                );
            }
        }

        let completed = Some(Condition::CompletedSuccessfully);
        let project_name = description.project.as_str();
        for one_shot in names(&required, completed) {
            if let Some(code) = wait(beam, project_name, service, one_shot).await? {
                return Err(ExecError {
                    message: format!(
                        "the dependency `{one_shot}` of service `{service}` exited with code \
                         {code}"
                    ),
                });
            }
        }
        for dependency in &optional_started {
            for one_shot in names(&[dependency], completed) {
                if let Some(code) = wait(beam, project_name, service, one_shot).await? {
                    warn_skipped(
                        beam,
                        dependency,
                        service,
                        "did not complete successfully",
                        &format!("its container exited with code {code}"),
                    );
                }
            }
        }
        Ok(())
    }

    /// Puts every dependency recorded back in the state it had: see
    /// `ComposeExecutor::shutdown`.
    ///
    /// A project whose start was interrupted is released a second time once
    /// `CREATION_SETTLE` has passed, for a creation or a start its `up` had
    /// sent the daemon before being killed. Accepted limit: that second pass
    /// is time-based, so an operation the daemon completes later than that
    /// still leaves its container behind (labelled with the project, and
    /// removed by the next `docker compose down`).
    pub async fn release(&self) -> Result<(), ExecError> {
        let mut records: Vec<_> = self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(name, record)| (name.clone(), Arc::clone(record)))
            .collect();
        records.sort_by(|(left, _), (right, _)| left.cmp(right));

        let mut failures = Vec::new();
        for (name, record) in records {
            let mut record = record.lock().await;
            let (removals, stops) = record.release_plan();
            record.started.clear();
            let interrupted = std::mem::take(&mut record.interrupted);
            let project = Project {
                dir: record.dir.clone(),
                file: record.file.clone(),
            };
            let passes = if interrupted { 2 } else { 1 };
            for pass in 0..passes {
                if pass > 0 {
                    tokio::time::sleep(CREATION_SETTLE).await;
                }
                for (args, services) in [
                    (&["rm", "-s", "-f"][..], &removals),
                    (&["stop"][..], &stops),
                ] {
                    if services.is_empty() {
                        continue;
                    }
                    if let Err(cause) = release_services(&project, args, services).await {
                        let failure = format!(
                            "could not release the dependencies of compose project \
                             `{name}`: {cause}"
                        );
                        // The second pass of an interrupted start may fail
                        // the same way as the first.
                        if !failures.contains(&failure) {
                            failures.push(failure);
                        }
                    }
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ExecError {
                message: failures.join("; "),
            })
        }
    }
}

/// `docker compose up -d --no-recreate <extra> <services>`, skipped when
/// `services` is empty: the exit code of the `up` when it is not 0.
///
/// `--parallel 1` makes Compose wait for each operation it sent the daemon
/// before sending the next: run in parallel, a creation that fails (an image
/// that does not exist) makes `up` exit while the daemon is still creating a
/// sibling, whose container then appears after the `up`, too late for a
/// removal at shutdown to see it. An `up` that did not run to its end (it
/// was cancelled, or the future of the session start was dropped, which
/// kills it) may still have had one such operation in flight: the record
/// is marked before the spawn and the mark kept, for `shutdown` to look
/// again. So is an `up` that ends once the beam is cancelled: a terminal
/// Ctrl-C reaches it too, and it can exit on the signal first.
async fn up(
    project: &Project,
    beam: &BeamContext,
    record: &mut ProjectRecord,
    extra: &[&str],
    services: &[&str],
) -> Result<Option<i32>, ExecError> {
    if services.is_empty() {
        return Ok(None);
    }
    let mut args = vec!["--parallel", "1", "up", "-d", "--no-recreate"];
    args.extend_from_slice(extra);
    args.extend_from_slice(services);
    // A successful `up` has nothing worth keeping on stdout.
    let previously = record.up_started();
    let up = run_step(project.command(&args), beam).await?;
    record.up_finished(previously, beam.cancel.is_cancelled());
    Ok(exit_code_if_failed(up.status))
}

/// Waits until every container of the one-shot `one_shot` of the Compose
/// project `project` has exited, and returns the first exit code that is
/// not 0. The containers are found by Compose's labels, one-off containers
/// (`docker compose run`) left out, and awaited with `docker wait`, which
/// answers for a container that already exited as for a running one.
/// `service` is the beam's service, for the error messages.
async fn wait(
    beam: &BeamContext,
    project: &str,
    service: &str,
    one_shot: &str,
) -> Result<Option<i32>, ExecError> {
    let project_label = format!("label=com.docker.compose.project={project}");
    let service_label = format!("label=com.docker.compose.service={one_shot}");
    let ps_args = [
        "ps",
        "-a",
        "-q",
        "--no-trunc",
        "--filter",
        project_label.as_str(),
        "--filter",
        service_label.as_str(),
        "--filter",
        "label=com.docker.compose.oneoff=False",
    ];
    let ps = run_step(docker(&ps_args), beam).await?;
    let containers = docker_stdout(ps, "ps", service, one_shot)?;
    let containers: Vec<&str> = containers.split_whitespace().collect();
    if containers.is_empty() {
        return Err(ExecError {
            message: format!(
                "the dependency `{one_shot}` of service `{service}` has no container to wait \
                 for"
            ),
        });
    }

    let mut wait_args = vec!["wait"];
    wait_args.extend_from_slice(&containers);
    let wait = run_step(docker(&wait_args), beam).await?;
    // One exit code per container, in order.
    let codes = docker_stdout(wait, "wait", service, one_shot)?;
    for line in codes.lines() {
        let code: i32 = line.trim().parse().map_err(|_| ExecError {
            message: format!(
                "`docker wait` printed `{line}` instead of an exit code for the dependency \
                 `{one_shot}` of service `{service}`"
            ),
        })?;
        if code != 0 {
            return Ok(Some(code));
        }
    }
    Ok(None)
}

/// `docker <args>`, its standard streams closed until [`run_step`] pipes
/// them.
fn docker(args: &[&str]) -> Command {
    let mut command = Command::new("docker");
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// The stdout of the `docker <subcommand>` step that served the dependency
/// `one_shot` of `service`, or why that step failed. Docker's own message is
/// already on the beam's output.
fn docker_stdout(
    outcome: StepOutcome,
    subcommand: &str,
    service: &str,
    one_shot: &str,
) -> Result<String, ExecError> {
    if !outcome.status.success() {
        let code = outcome.status.code().unwrap_or(-1);
        return Err(ExecError {
            message: format!(
                "docker {subcommand} exited with code {code} for the dependency `{one_shot}` \
                 of service `{service}`"
            ),
        });
    }
    outcome.stdout.map_err(|cause| ExecError {
        message: format!("could not read the output of `docker {subcommand}`: {cause}"),
    })
}

fn exit_code_if_failed(status: std::process::ExitStatus) -> Option<i32> {
    (!status.success()).then(|| status.code().unwrap_or(-1))
}

/// The services of `dependencies` whose condition is `wanted`, or all of
/// them when `wanted` is `None`.
fn names<'a>(dependencies: &[&'a Dependency], wanted: Option<Condition>) -> Vec<&'a str> {
    dependencies
        .iter()
        .filter(|dependency| wanted.is_none_or(|wanted| dependency.condition == wanted))
        .map(|dependency| dependency.service.as_str())
        .collect()
}

/// What a failed `up` of an optional dependency reports in its warning.
fn up_exited(code: i32) -> String {
    format!("docker compose up exited with code {code}")
}

fn up_failed(code: i32, service: &str) -> ExecError {
    ExecError {
        message: format!(
            "docker compose up exited with code {code} for the dependencies of service \
             `{service}`"
        ),
    }
}

/// The one line on the beam's output saying that the optional `dependency`
/// is left out: what went wrong, and `cause`, the exit code that showed it.
/// Compose's own message, if any, is already there.
fn warn_skipped(
    beam: &BeamContext,
    dependency: &Dependency,
    service: &str,
    what: &str,
    cause: &str,
) {
    let _ = beam.output.send(OutputLine {
        stream: Stream::Stderr,
        text: format!(
            "optional dependency `{}` of service `{service}` {what} ({cause}): continuing \
             without it",
            dependency.service
        ),
    });
}

/// Runs `docker compose <args> <services>` for `shutdown`, its stdout
/// discarded. A failure is described by Compose's stderr, or by the exit
/// code when that is empty.
async fn release_services(
    project: &Project,
    args: &[&str],
    services: &[String],
) -> Result<(), String> {
    let output = project
        .command(args)
        .args(services)
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| spawn_error(error, COMPOSE_REQUIREMENT).message)?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        let code = output.status.code().unwrap_or(-1);
        Err(format!(
            "docker compose {} exited with code {code}",
            args[0]
        ))
    } else {
        Err(stderr)
    }
}
