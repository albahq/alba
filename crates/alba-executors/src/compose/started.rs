//! The bookkeeping of the dependencies the compose executor starts: what
//! state each one was in before Alba started it, so that
//! `Executor::shutdown` can put it back. A dependency that was already
//! running is not Alba's and is never recorded.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use crate::ExecError;

/// The state a dependency Alba started was in before: what `shutdown`
/// restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Before {
    /// It had no container: `shutdown` removes the one Alba started.
    Absent,
    /// It had a container that was not running: `shutdown` stops it again.
    Stopped,
}

/// What `docker compose ps -a --format json` printed, as service -> is it running.
///
/// Only a container `created`, `exited` or `dead` counts as not running: any
/// other state (`running`, `restarting`, `paused`, ...) means the container
/// is up in some form the developer chose, so it is left alone and never
/// Alba's to stop. A service with several containers is running when one of
/// them is.
pub(super) fn running_by_service(ps_output: &str) -> Result<HashMap<String, bool>, ExecError> {
    let mut states = HashMap::new();
    for line in ps_output.lines().filter(|line| !line.trim().is_empty()) {
        let container: PsLine = serde_json::from_str(line).map_err(|error| ExecError {
            message: format!(
                "cannot read the containers of the project from `docker compose ps`: \
                 {error} in `{line}`"
            ),
        })?;
        let running = !matches!(container.state.as_str(), "created" | "exited" | "dead");
        *states.entry(container.service).or_insert(false) |= running;
    }
    Ok(states)
}

/// The part of one line of `docker compose ps --format json` read here.
#[derive(serde::Deserialize)]
struct PsLine {
    #[serde(rename = "Service")]
    service: String,
    #[serde(rename = "State")]
    state: String,
}

/// The state to restore for each dependency that is not running. Running ones are absent from the result.
pub(super) fn to_restore(
    dependencies: &[String],
    states: &HashMap<String, bool>,
) -> Vec<(String, Before)> {
    dependencies
        .iter()
        .filter_map(|service| match states.get(service) {
            None => Some((service.clone(), Before::Absent)),
            Some(false) => Some((service.clone(), Before::Stopped)),
            Some(true) => None,
        })
        .collect()
}

/// The dependencies Alba started in one Compose project, and how to address that project again.
#[derive(Debug)]
pub(super) struct ProjectRecord {
    pub dir: PathBuf,
    pub file: Option<PathBuf>,
    pub started: BTreeMap<String, Before>,
    /// Whether an `up` of this project's dependencies was cut short since
    /// the last shutdown, and may have left an operation in flight.
    pub interrupted: bool,
}

impl ProjectRecord {
    /// Adds dependencies not yet recorded. One already recorded keeps its first state.
    pub fn record(&mut self, restore: Vec<(String, Before)>) {
        for (service, before) in restore {
            self.started.entry(service).or_insert(before);
        }
    }

    /// The services to `rm -s -f` and the services to `stop`, each sorted.
    pub fn release_plan(&self) -> (Vec<String>, Vec<String>) {
        let services = |state: Before| {
            self.started
                .iter()
                .filter(move |(_, before)| **before == state)
                .map(|(service, _)| service.clone())
                .collect::<Vec<_>>()
        };
        (services(Before::Absent), services(Before::Stopped))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn record() -> ProjectRecord {
        ProjectRecord {
            dir: PathBuf::from("/project"),
            file: None,
            started: BTreeMap::new(),
            interrupted: false,
        }
    }

    #[test]
    fn running_by_service_reads_one_object_per_line() {
        let output = "{\"Service\":\"db\",\"State\":\"running\",\"Name\":\"p-db-1\"}\n\
                      {\"Service\":\"cache\",\"State\":\"exited\",\"Name\":\"p-cache-1\"}\n";

        let states = running_by_service(output).unwrap();

        assert_eq!(
            states,
            HashMap::from([("db".to_string(), true), ("cache".to_string(), false)])
        );
        assert_eq!(running_by_service("").unwrap(), HashMap::new());
    }

    #[test]
    fn a_container_up_in_any_form_counts_as_running() {
        // A paused or restarting dependency is the developer's: recording it
        // as stopped would have `shutdown` stop it.
        let output = "{\"Service\":\"db\",\"State\":\"paused\"}\n\
                      {\"Service\":\"cache\",\"State\":\"restarting\"}\n\
                      {\"Service\":\"init\",\"State\":\"created\"}\n\
                      {\"Service\":\"worker\",\"State\":\"exited\"}\n\
                      {\"Service\":\"worker\",\"State\":\"running\"}\n";

        let states = running_by_service(output).unwrap();

        assert_eq!(
            states,
            HashMap::from([
                ("db".to_string(), true),
                ("cache".to_string(), true),
                ("init".to_string(), false),
                ("worker".to_string(), true),
            ])
        );
    }

    #[test]
    fn a_running_dependency_is_not_alba_s_to_restore() {
        let states = HashMap::from([("db".to_string(), true)]);

        assert_eq!(
            to_restore(&names(&["cache", "db"]), &states),
            vec![("cache".to_string(), Before::Absent)]
        );
    }

    #[test]
    fn a_stopped_dependency_is_restored_to_stopped() {
        let states = HashMap::from([("cache".to_string(), false)]);

        assert_eq!(
            to_restore(&names(&["cache"]), &states),
            vec![("cache".to_string(), Before::Stopped)]
        );
    }

    #[test]
    fn a_dependency_recorded_twice_keeps_its_first_state() {
        let mut record = record();

        record.record(vec![("db".to_string(), Before::Absent)]);
        // What a second beam sees once the first was cancelled mid-start:
        // the container Alba created, not running.
        record.record(vec![("db".to_string(), Before::Stopped)]);

        assert_eq!(record.release_plan(), (names(&["db"]), vec![]));
    }

    #[test]
    fn the_release_plan_separates_removals_from_stops() {
        let mut record = record();

        record.record(vec![
            ("c".to_string(), Before::Absent),
            ("b".to_string(), Before::Stopped),
            ("a".to_string(), Before::Absent),
        ]);

        assert_eq!(record.release_plan(), (names(&["a", "c"]), names(&["b"])));
    }
}
