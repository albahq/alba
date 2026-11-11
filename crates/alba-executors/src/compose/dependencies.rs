//! The dependencies of a Compose service: brought up before the service's
//! own container, recorded with the state each one had before when Alba
//! started it, and put back in that state when the executor shuts down. The
//! pure bookkeeping lives in [`super::started`].

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use crate::container::spawn_error;
use crate::{BeamContext, ExecError};

use super::service::ServiceDescription;
use super::started::{ProjectRecord, running_by_service, to_restore};
use super::{
    COMPOSE_REQUIREMENT, CREATION_SETTLE, Project, run_step, start_cancelled, step_failed,
    step_stdout,
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

        let ps = run_step(project.command(&["ps", "-a", "--format", "json"]), beam).await?;
        if !ps.status.success() {
            return Err(step_failed("ps", ps.status, service));
        }
        let states = running_by_service(&step_stdout(ps, "ps")?)?;
        record.record(to_restore(&description.dependencies, &states));

        // See `DependencyStart` for why the one-shots are started apart.
        let start = &description.start;
        start_up(
            project,
            beam,
            &mut record,
            &["--wait"],
            &start.waited,
            service,
        )
        .await?;
        start_up(
            project,
            beam,
            &mut record,
            &[],
            &start.one_shots_to_start,
            service,
        )
        .await?;
        for one_shot in &start.one_shots {
            // Its stdout only repeats the exit code.
            let wait = run_step(project.command(&["wait", one_shot]), beam).await?;
            if !wait.status.success() {
                let code = wait.status.code().unwrap_or(-1);
                return Err(ExecError {
                    message: format!(
                        "docker compose wait exited with code {code} for the dependency \
                         `{one_shot}` of service `{service}`"
                    ),
                });
            }
        }
        Ok(())
    }

    /// Puts every dependency recorded back in the state it had: see
    /// `ComposeExecutor::shutdown`.
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

/// `docker compose up -d --no-recreate <extra> <services>` for the
/// dependencies of `service`, skipped when `services` is empty.
///
/// `--parallel 1` makes Compose wait for each operation it sent the daemon
/// before sending the next: run in parallel, a creation that fails (an image
/// that does not exist) makes `up` exit while the daemon is still creating a
/// sibling, whose container then appears after the `up`, too late for a
/// removal at shutdown to see it. An `up` that did not run to its end (it
/// was cancelled) may still have had one such operation in flight: the
/// record says so, for `shutdown` to look again.
async fn start_up(
    project: &Project,
    beam: &BeamContext,
    record: &mut ProjectRecord,
    extra: &[&str],
    services: &[String],
    service: &str,
) -> Result<(), ExecError> {
    if services.is_empty() {
        return Ok(());
    }
    let mut args = vec!["--parallel", "1", "up", "-d", "--no-recreate"];
    args.extend_from_slice(extra);
    args.extend(services.iter().map(String::as_str));
    // A successful `up` has nothing worth keeping on stdout.
    let up = match run_step(project.command(&args), beam).await {
        Ok(up) => up,
        Err(error) => {
            record.interrupted = true;
            return Err(error);
        }
    };
    if !up.status.success() {
        let code = up.status.code().unwrap_or(-1);
        return Err(ExecError {
            message: format!(
                "docker compose up exited with code {code} for the dependencies of \
                 service `{service}`"
            ),
        });
    }
    Ok(())
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
