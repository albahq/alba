//! What the compose executor needs to know about one Compose service, read
//! from `docker compose config --format json <service>`: the project it
//! belongs to, its bind mounts, whether it declares a build, the services
//! it depends on, and the condition each direct dependency must meet. Also
//! where a host directory is found inside the service's container, derived
//! from those bind mounts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::ExecError;

/// One bind mount of a service: the host directory `source`, absolute and
/// symlink-resolved as `docker compose config` reports it, mounted on the
/// container path `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bind {
    pub source: PathBuf,
    pub target: String,
}

/// One Compose service, as `docker compose config` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ServiceDescription {
    /// The Compose project's name (`name` in the configuration).
    pub project: String,
    pub service: String,
    /// The service's bind mounts, in declaration order. Named volumes and
    /// other mount types have no host directory and are left out.
    pub binds: Vec<Bind>,
    /// Whether the service declares a `build`.
    pub has_build: bool,
    /// The services named in the service's own `depends_on`, sorted by
    /// name.
    pub direct_dependencies: Vec<Dependency>,
    /// Every service the service depends on, directly or not, sorted, the
    /// service itself excluded.
    pub dependencies: Vec<String>,
}

/// What a dependency must have reached before the service's container
/// starts: its `depends_on` condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Condition {
    /// `service_started`, also what the short syntax means.
    Started,
    /// `service_healthy`: its healthcheck passes.
    Healthy,
    /// `service_completed_successfully`: it ran to its end, exiting 0.
    CompletedSuccessfully,
}

/// One service named in another's `depends_on`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Dependency {
    pub service: String,
    pub condition: Condition,
}

impl Dependency {
    pub fn new(service: &str, condition: Condition) -> Self {
        Self {
            service: service.to_string(),
            condition,
        }
    }
}

/// The part of `docker compose config --format json` this module reads.
#[derive(serde::Deserialize)]
struct Config {
    name: String,
    services: BTreeMap<String, Service>,
}

#[derive(serde::Deserialize)]
struct Service {
    #[serde(default)]
    build: Option<serde_json::Value>,
    #[serde(default)]
    volumes: Vec<Volume>,
    /// An object keyed by service name.
    #[serde(default)]
    depends_on: BTreeMap<String, DependsOn>,
}

#[derive(serde::Deserialize)]
struct DependsOn {
    /// `service_started` when the short syntax was used.
    #[serde(default)]
    condition: Option<String>,
}

#[derive(serde::Deserialize)]
struct Volume {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    source: Option<String>,
    target: String,
}

impl ServiceDescription {
    /// Reads the description of `service` from the JSON `docker compose
    /// config --format json <service>` printed.
    pub fn parse(config_json: &str, service: &str) -> Result<Self, ExecError> {
        let unreadable = |cause: String| ExecError {
            message: format!(
                "cannot read the description of service `{service}` from \
                 `docker compose config`: {cause}"
            ),
        };

        let config: Config =
            serde_json::from_str(config_json).map_err(|error| unreadable(error.to_string()))?;
        let entry = config
            .services
            .get(service)
            .ok_or_else(|| unreadable(format!("the configuration has no service `{service}`")))?;

        let mut binds = Vec::new();
        for volume in entry.volumes.iter().filter(|volume| volume.kind == "bind") {
            let source = volume.source.as_ref().ok_or_else(|| {
                unreadable(format!(
                    "the bind mount on `{}` has no source",
                    volume.target
                ))
            })?;
            binds.push(Bind {
                source: PathBuf::from(source),
                target: volume.target.clone(),
            });
        }

        Ok(Self {
            project: config.name.clone(),
            service: service.to_string(),
            binds,
            has_build: entry.build.is_some(),
            direct_dependencies: direct_dependencies(entry).map_err(unreadable)?,
            dependencies: dependency_closure(&config.services, service),
        })
    }

    /// Where `host_cwd` is found inside the service's container: the bind
    /// mount covering it with the longest source wins, and the rest of the
    /// path follows that mount's target. Both sides are compared in
    /// canonical form, since `docker compose config` reports resolved paths
    /// (`/private/tmp/...` where the shell says `/tmp/...` on macOS). A
    /// directory no bind mount covers is an error: running a command in the
    /// wrong directory silently would be worse than failing.
    pub fn container_cwd(&self, host_cwd: &Path) -> Result<String, ExecError> {
        let cwd = canonical(host_cwd);
        let covering = self
            .binds
            .iter()
            .enumerate()
            .filter_map(|(index, bind)| {
                let source = canonical(&bind.source);
                let rest = cwd.strip_prefix(&source).ok()?.to_path_buf();
                Some((
                    source.components().count(),
                    std::cmp::Reverse(index),
                    bind,
                    rest,
                ))
            })
            .max_by_key(|(depth, first, _, _)| (*depth, *first));

        let Some((_, _, bind, rest)) = covering else {
            return Err(ExecError {
                message: format!(
                    "no bind mount of service `{}` covers `{}` ({})",
                    self.service,
                    cwd.display(),
                    self.bind_list()
                ),
            });
        };

        let rest: Vec<String> = rest
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        if rest.is_empty() {
            return Ok(bind.target.clone());
        }
        Ok(format!(
            "{}/{}",
            bind.target.trim_end_matches('/'),
            rest.join("/")
        ))
    }

    /// The service's bind mounts, for an error message.
    fn bind_list(&self) -> String {
        if self.binds.is_empty() {
            return "the service has no bind mount".to_string();
        }
        let binds: Vec<String> = self
            .binds
            .iter()
            .map(|bind| format!("{} -> {}", bind.source.display(), bind.target))
            .collect();
        format!("bind mounts: {}", binds.join(", "))
    }
}

/// `path` with its symlinks resolved, or as given when it cannot be resolved
/// (it does not exist on this host, for example).
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// The dependencies `entry` names in its `depends_on`, sorted by name. A
/// condition Compose does not define is refused rather than guessed.
fn direct_dependencies(entry: &Service) -> Result<Vec<Dependency>, String> {
    entry
        .depends_on
        .iter()
        .map(|(service, depends_on)| {
            let condition = match depends_on.condition.as_deref() {
                None | Some("service_started") => Condition::Started,
                Some("service_healthy") => Condition::Healthy,
                Some("service_completed_successfully") => Condition::CompletedSuccessfully,
                Some(other) => {
                    return Err(format!(
                        "the dependency `{service}` has the unknown condition `{other}`"
                    ));
                }
            };
            Ok(Dependency::new(service, condition))
        })
        .collect()
}

/// Every service `service` depends on, directly or through another
/// dependency, sorted and without `service` itself. A dependency missing
/// from `services` is kept but not followed; a cycle (which Compose itself
/// rejects) cannot loop.
fn dependency_closure(services: &BTreeMap<String, Service>, service: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![service.to_string()];
    while let Some(current) = pending.pop() {
        let Some(entry) = services.get(&current) else {
            continue;
        };
        for dependency in entry.depends_on.keys() {
            if dependency != service && seen.insert(dependency.clone()) {
                pending.push(dependency.clone());
            }
        }
    }
    seen.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// The configuration `docker compose config --format json tool` prints for
    /// a service `tool` bind-mounting `/repo` on `/app`, with a named volume
    /// on `/app/node_modules`, depending on `db`, which depends on `cache`.
    fn fixture() -> serde_json::Value {
        serde_json::json!({
            "name": "albaspike",
            "services": {
                "tool": {
                    "image": "alpine:3",
                    "volumes": [
                        {"type": "bind", "source": "/repo", "target": "/app", "bind": {}},
                        {"type": "volume", "source": "nm", "target": "/app/node_modules", "volume": {}}
                    ],
                    "depends_on": {"db": {"condition": "service_started", "required": true}}
                },
                "db": {
                    "image": "alpine:3",
                    "depends_on": {"cache": {"condition": "service_started", "required": true}}
                },
                "cache": {"image": "alpine:3"}
            }
        })
    }

    fn parse(config: &serde_json::Value, service: &str) -> ServiceDescription {
        ServiceDescription::parse(&config.to_string(), service).unwrap()
    }

    /// A service `tool` whose only mounts are the given `(source, target)`
    /// binds.
    fn with_binds(binds: &[(&str, &str)]) -> ServiceDescription {
        let volumes: Vec<_> = binds
            .iter()
            .map(|(source, target)| {
                serde_json::json!({"type": "bind", "source": source, "target": target})
            })
            .collect();
        parse(
            &serde_json::json!({
                "name": "albaspike",
                "services": {"tool": {"image": "alpine:3", "volumes": volumes}}
            }),
            "tool",
        )
    }

    fn cwd_error(description: &ServiceDescription, host_cwd: &str) -> String {
        description
            .container_cwd(Path::new(host_cwd))
            .unwrap_err()
            .message
    }

    #[test]
    fn parse_reads_the_project_the_binds_and_the_build_flag() {
        let description = parse(&fixture(), "tool");
        assert_eq!(description.project, "albaspike");
        assert_eq!(description.service, "tool");
        assert_eq!(description.binds.len(), 1);
        assert_eq!(description.binds[0].source, PathBuf::from("/repo"));
        assert_eq!(description.binds[0].target, "/app");
        assert!(!description.has_build);

        let mut config = fixture();
        config["services"]["tool"]["build"] = serde_json::json!({"context": "/repo"});
        assert!(parse(&config, "tool").has_build);
    }

    #[test]
    fn parse_separates_direct_dependencies_from_their_closure() {
        let description = parse(&fixture(), "tool");
        assert_eq!(
            description.direct_dependencies,
            vec![Dependency::new("db", Condition::Started)]
        );
        assert_eq!(description.dependencies, vec!["cache", "db"]);
    }

    #[test]
    fn parse_classifies_each_direct_dependency_by_its_condition() {
        let condition =
            |condition: &str| serde_json::json!({"condition": condition, "required": true});
        let config = serde_json::json!({
            "name": "albaspike",
            "services": {
                "tool": {"depends_on": {
                    "db": condition("service_started"),
                    "cache": condition("service_healthy"),
                    "migrate": condition("service_completed_successfully"),
                    // What `config` prints for a dependency without a condition.
                    "queue": {"required": true}
                }},
                "db": {}, "cache": {}, "migrate": {}, "queue": {}
            }
        });

        let description = parse(&config, "tool");

        assert_eq!(
            description.direct_dependencies,
            vec![
                Dependency::new("cache", Condition::Healthy),
                Dependency::new("db", Condition::Started),
                Dependency::new("migrate", Condition::CompletedSuccessfully),
                Dependency::new("queue", Condition::Started),
            ]
        );
    }

    #[test]
    fn parse_rejects_an_unknown_condition() {
        let mut config = fixture();
        config["services"]["tool"]["depends_on"]["db"]["condition"] =
            serde_json::json!("service_ready");

        let error = ServiceDescription::parse(&config.to_string(), "tool").unwrap_err();

        assert!(
            error
                .message
                .ends_with("the dependency `db` has the unknown condition `service_ready`"),
            "unexpected message: {}",
            error.message
        );
    }

    #[test]
    fn parse_survives_a_dependency_cycle_free_diamond() {
        let started = serde_json::json!({"condition": "service_started"});
        let config = serde_json::json!({
            "name": "albaspike",
            "services": {
                "tool": {"depends_on": {"b": started, "a": started}},
                "a": {"depends_on": {"c": started}},
                "b": {"depends_on": {"c": started}},
                "c": {}
            }
        });
        let description = parse(&config, "tool");
        assert_eq!(
            description.direct_dependencies,
            vec![
                Dependency::new("a", Condition::Started),
                Dependency::new("b", Condition::Started),
            ]
        );
        assert_eq!(description.dependencies, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_rejects_a_config_without_the_service() {
        let error = ServiceDescription::parse(&fixture().to_string(), "php").unwrap_err();
        assert!(
            error.message.starts_with(
                "cannot read the description of service `php` from `docker compose config`: "
            ),
            "unexpected message: {}",
            error.message
        );

        let error = ServiceDescription::parse("not json", "php").unwrap_err();
        assert!(
            error.message.starts_with(
                "cannot read the description of service `php` from `docker compose config`: "
            ),
            "unexpected message: {}",
            error.message
        );
    }

    #[test]
    fn a_directory_under_a_bind_maps_under_its_target() {
        let description = parse(&fixture(), "tool");
        assert_eq!(
            description
                .container_cwd(Path::new("/repo/modules/facturation"))
                .unwrap(),
            "/app/modules/facturation"
        );
        assert_eq!(
            description.container_cwd(Path::new("/repo")).unwrap(),
            "/app"
        );
    }

    #[test]
    fn the_longest_covering_bind_wins() {
        let description = with_binds(&[("/repo", "/app"), ("/repo/apps/site", "/site")]);
        assert_eq!(
            description
                .container_cwd(Path::new("/repo/apps/site/src"))
                .unwrap(),
            "/site/src"
        );
    }

    #[test]
    fn a_bind_is_a_prefix_by_component_not_by_text() {
        let description = with_binds(&[("/repo", "/app")]);
        assert!(
            description
                .container_cwd(Path::new("/repository/x"))
                .is_err()
        );
    }

    #[test]
    fn a_directory_no_bind_covers_is_an_error_listing_the_binds() {
        let mut description = with_binds(&[("/repo", "/app"), ("/cache", "/c")]);
        description.service = "php".to_string();
        assert_eq!(
            cwd_error(&description, "/host/elsewhere"),
            "no bind mount of service `php` covers `/host/elsewhere` \
             (bind mounts: /repo -> /app, /cache -> /c)"
        );
    }

    #[test]
    fn a_service_with_only_named_volumes_covers_nothing() {
        let mut config = fixture();
        config["services"]["tool"]["volumes"] = serde_json::json!([
            {"type": "volume", "source": "nm", "target": "/app/node_modules", "volume": {}}
        ]);
        let description = parse(&config, "tool");
        let message = cwd_error(&description, "/repo");
        assert!(
            message.ends_with("(the service has no bind mount)"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn a_bind_on_the_container_root_does_not_double_the_slash() {
        let description = with_binds(&[("/repo", "/")]);
        assert_eq!(
            description.container_cwd(Path::new("/repo/a")).unwrap(),
            "/a"
        );
        assert_eq!(description.container_cwd(Path::new("/repo")).unwrap(), "/");
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_reached_through_a_symlink_still_maps() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::os::unix::fs::symlink(&real, temp.path().join("link")).unwrap();

        let source = real.canonicalize().unwrap();
        let description = with_binds(&[(source.to_str().unwrap(), "/work")]);
        assert_eq!(
            description
                .container_cwd(&temp.path().join("link").join("sub"))
                .unwrap(),
            "/work/sub"
        );
    }
}
