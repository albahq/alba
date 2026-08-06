//! [`DockerExecutor`]: runs a beam's commands inside a single, per-session
//! container via the `docker` CLI.
//!
//! One container is started per beam session and kept alive for the whole
//! session by a dormant `sh -c "sleep 2147483647"` process — a documented
//! prerequisite that the image provides `/bin/sh`. Each [`CommandSpec`] then
//! runs through `docker exec` against that same container, so state a
//! command leaves behind (files, environment set up by a previous command)
//! is visible to the next one in the same beam.
//!
//! This shells out to the `docker` CLI rather than talking to the daemon
//! through an API client, so it works unmodified against Docker Desktop,
//! remote docker contexts, and API-compatible lookalikes (anything `docker`
//! itself is configured to talk to). The container is started with `--rm`
//! so that even a crashed Alba process leaves cleanup to the daemon instead
//! of leaking a stopped container forever.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor, Stream,
};

/// How long to wait for `docker stop` to succeed gracefully before falling
/// back to a forceful kill of the `docker exec` process itself.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// Runs beam commands inside a per-session docker container. See the module
/// doc comment for the container lifecycle.
pub struct DockerExecutor {
    project_root: PathBuf,
}

impl DockerExecutor {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[derive(Debug, PartialEq, serde::Deserialize)]
struct DockerConfig {
    image: String,
    // `#[serde(default)]` alone only kicks in when the key is absent; the
    // DSL's `volumes ["h:/c"]` is optional, so the engine's rendered JSON
    // carries an explicit `"volumes": null` rather than omitting the key,
    // which plain `default` does not tolerate for a non-`Option` field.
    #[serde(default, deserialize_with = "null_as_default")]
    volumes: Vec<String>,
    #[serde(default)]
    workdir: Option<String>,
}

/// Treats an explicit JSON `null` the same as an absent field, deserializing
/// it to `T::default()` instead of failing.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

#[async_trait::async_trait]
impl Executor for DockerExecutor {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError> {
        let config: DockerConfig =
            serde_json::from_value(beam.options).map_err(|error| ExecError {
                message: format!("invalid docker options: {error}"),
            })?;
        let name = container_name(&beam.beam);
        let mapping = PathMapping::new(&self.project_root, config.workdir.clone());

        let args = run_args(&config, &self.project_root, &name, &beam.beam);
        let mut child = Command::new("docker")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(docker_spawn_error)?;

        let mut stdout = child
            .stdout
            .take()
            .expect("child spawned with piped stdout");
        let stderr = child
            .stderr
            .take()
            .expect("child spawned with piped stderr");

        // `docker run -d` prints only the new container id on stdout, so it
        // is captured directly rather than forwarded through
        // `stream_lines`. stderr, on the other hand, is where an image pull
        // reports its progress, which is worth showing on the beam's output
        // while we wait rather than staying silent until it either succeeds
        // or fails.
        let stdout_task = tokio::spawn(async move {
            let mut buf = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut buf).await;
            buf
        });
        let stderr_task = tokio::spawn(stream_lines(stderr, Stream::Stderr, beam.output.clone()));

        let status = tokio::select! {
            result = child.wait() => result.map_err(|error| ExecError {
                message: format!("failed to wait for `docker run`: {error}"),
            })?,
            _ = beam.cancel.cancelled() => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                remove_container_fire_and_forget(&name);
                stdout_task.abort();
                stderr_task.abort();
                return Err(ExecError {
                    message: "container start cancelled".into(),
                });
            }
        };

        let container_id = stdout_task.await.unwrap_or_default();
        let _ = stderr_task.await;

        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Err(ExecError {
                message: format!(
                    "docker run exited with code {code} for image `{}`",
                    config.image
                ),
            });
        }

        Ok(Box::new(DockerSession {
            container: container_id.trim().to_string(),
            name,
            mapping,
        }))
    }
}

/// The session opened for one beam: `container` (its docker-assigned id) is
/// the target of every `docker exec`/`stop`/`rm` this session issues;
/// `name` is kept alongside it (it is what `--name` was given at `run`
/// time) purely for diagnosability — e.g. in a debug dump of a stuck
/// session — since the id alone is not human-legible.
#[derive(Debug)]
struct DockerSession {
    container: String,
    #[allow(dead_code)]
    name: String,
    mapping: PathMapping,
}

#[async_trait::async_trait]
impl ExecSession for DockerSession {
    async fn execute(
        &mut self,
        cmd: CommandSpec,
        ctx: ExecContext,
    ) -> Result<ExecResult, ExecError> {
        let cwd = self.mapping.container_cwd(&cmd.cwd);
        let args = exec_args(&self.container, &cwd, &cmd.env, &cmd.command);
        let mut child = Command::new("docker")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(docker_spawn_error)?;

        let stdout = child
            .stdout
            .take()
            .expect("child spawned with piped stdout");
        let stderr = child
            .stderr
            .take()
            .expect("child spawned with piped stderr");

        let mut stdout_task =
            tokio::spawn(stream_lines(stdout, Stream::Stdout, ctx.output.clone()));
        let mut stderr_task =
            tokio::spawn(stream_lines(stderr, Stream::Stderr, ctx.output.clone()));

        let status = tokio::select! {
            result = child.wait() => result.map_err(|error| ExecError {
                message: format!("failed to wait for `docker exec`: {error}"),
            })?,
            _ = ctx.cancel.cancelled() => {
                run_quiet(&["stop", "--time", "5", &self.container]).await;
                run_quiet(&["rm", "-f", &self.container]).await;
                match tokio::time::timeout(STOP_GRACE, child.wait()).await {
                    Ok(result) => result.map_err(|error| ExecError {
                        message: format!("failed to wait for cancelled `docker exec`: {error}"),
                    })?,
                    Err(_elapsed) => {
                        let _ = child.kill().await;
                        child.wait().await.map_err(|error| ExecError {
                            message: format!("failed to wait for killed `docker exec`: {error}"),
                        })?
                    }
                }
            }
        };

        let _ = tokio::join!(&mut stdout_task, &mut stderr_task);

        Ok(ExecResult {
            exit_code: status.code().unwrap_or(-1),
        })
    }

    async fn close(self: Box<Self>) -> Result<(), ExecError> {
        let mut child = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(docker_spawn_error)?;
        let status = child.wait().await.map_err(|error| ExecError {
            message: format!("failed to wait for `docker rm`: {error}"),
        })?;
        if !status.success() {
            let code = status.code().unwrap_or(-1);
            return Err(ExecError {
                message: format!("docker rm exited with code {code}"),
            });
        }
        Ok(())
    }
}

/// Maps a spawn failure of the `docker` binary itself (as opposed to a
/// non-zero exit from a `docker` subcommand) to a hint that the executor
/// needs `docker` on the `PATH`.
fn docker_spawn_error(error: std::io::Error) -> ExecError {
    ExecError {
        message: format!(
            "cannot run `docker`: {error} — the docker executor requires Docker on the PATH"
        ),
    }
}

/// Runs a `docker` subcommand to completion, discarding its output; used
/// for cleanup steps (`stop`, `rm -f`) whose own failure is not worth
/// surfacing since the caller has already decided the container is going
/// away regardless.
async fn run_quiet(args: &[&str]) {
    if let Ok(mut child) = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        let _ = child.wait().await;
    }
}

/// Best-effort, non-blocking `docker rm -f`: used when the container may
/// not have finished starting yet (a cancellation raced with `docker run`
/// itself), so there is nothing worth waiting on — the process is spawned
/// and immediately let go.
fn remove_container_fire_and_forget(name: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// A unique, docker-legal name for the container backing one beam session:
/// `alba-<sanitized beam>-<pid>-<counter>`. The pid and a process-local
/// counter together make names unique across concurrent Alba processes and
/// concurrent beams within one process (docker container names must be
/// unique on the host).
fn container_name(beam: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sanitized: String = beam
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("alba-{sanitized}-{}-{counter}", std::process::id())
}

/// Translates a command's working directory (a host path, since the engine
/// builds [`CommandSpec::cwd`] from the project root without knowing about
/// containers) into the path it corresponds to inside the container.
///
/// On unix the project root is bind-mounted at its own host path (see
/// `run_args`), so the host path is also the container path: no
/// translation needed. On windows the project root is mounted at
/// `/workspace` instead (docker cannot bind-mount a `C:\...` path at an
/// identical in-container path), so a host path under the project root
/// becomes the same path relative to `/workspace`; a path outside the
/// project root has no defined mapping and falls back to `/workspace`
/// itself. A beam-declared `workdir` always wins over this computation.
#[derive(Debug)]
struct PathMapping {
    // Only read on windows (see `container_cwd`'s `#[cfg(windows)]` branch);
    // on unix the host path needs no translation, so this field is inert
    // there.
    #[cfg_attr(unix, allow(dead_code))]
    project_root: PathBuf,
    workdir: Option<String>,
}

impl PathMapping {
    fn new(project_root: &Path, workdir: Option<String>) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            workdir,
        }
    }

    fn container_cwd(&self, cwd: &Path) -> String {
        if let Some(workdir) = &self.workdir {
            return workdir.clone();
        }

        #[cfg(unix)]
        {
            cwd.to_string_lossy().into_owned()
        }

        #[cfg(windows)]
        {
            match cwd.strip_prefix(&self.project_root) {
                Ok(relative) if relative.as_os_str().is_empty() => "/workspace".to_string(),
                Ok(relative) => {
                    let joined = relative
                        .components()
                        .map(|component| component.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    format!("/workspace/{joined}")
                }
                Err(_) => "/workspace".to_string(),
            }
        }
    }
}

/// Builds the argument vector for `docker run -d`, which starts the
/// dormant container backing one beam session (see the module doc
/// comment). The project root is bind-mounted so the container sees the
/// same files the beam's commands would see running on the host; the
/// beam's declared volumes are appended verbatim after it.
fn run_args(config: &DockerConfig, project_root: &Path, name: &str, beam: &str) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "-d".to_string(),
        "--rm".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--label".to_string(),
        format!("alba.beam={beam}"),
        "-v".to_string(),
    ];

    #[cfg(unix)]
    {
        let root = project_root.display().to_string();
        args.push(format!("{root}:{root}"));
    }
    #[cfg(windows)]
    {
        args.push(format!("{}:/workspace", project_root.display()));
    }

    for volume in &config.volumes {
        args.push("-v".to_string());
        args.push(volume.clone());
    }

    args.push(config.image.clone());
    args.push("sh".to_string());
    args.push("-c".to_string());
    args.push("sleep 2147483647".to_string());
    args
}

/// Builds the argument vector for `docker exec`, running one already
/// rendered command line inside the session's container via `sh -c`.
fn exec_args(container: &str, cwd: &str, env: &[(String, String)], command: &str) -> Vec<String> {
    let mut args = vec!["exec".to_string(), "-w".to_string(), cwd.to_string()];
    for (key, value) in env {
        args.push("-e".to_string());
        args.push(format!("{key}={value}"));
    }
    args.push(container.to_string());
    args.push("sh".to_string());
    args.push("-c".to_string());
    args.push(command.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_with_defaults() {
        let config: DockerConfig = serde_json::from_value(
            serde_json::json!({"image": "alpine:3", "volumes": null, "workdir": null}),
        )
        .unwrap();
        assert_eq!(
            config,
            DockerConfig {
                image: "alpine:3".into(),
                volumes: vec![],
                workdir: None,
            }
        );
    }

    #[test]
    fn run_args_mount_the_project_and_keep_the_container_dormant() {
        let config = DockerConfig {
            image: "alpine:3".into(),
            volumes: vec!["h:/c".into()],
            workdir: None,
        };
        let args = run_args(
            &config,
            std::path::Path::new("/proj"),
            "alba-build-1-0",
            "build",
        );
        #[cfg(unix)]
        assert_eq!(
            args,
            vec![
                "run",
                "-d",
                "--rm",
                "--name",
                "alba-build-1-0",
                "--label",
                "alba.beam=build",
                "-v",
                "/proj:/proj",
                "-v",
                "h:/c",
                "alpine:3",
                "sh",
                "-c",
                "sleep 2147483647",
            ]
        );
        #[cfg(windows)]
        assert!(
            args.iter().any(|arg| arg.ends_with(":/workspace")),
            "the windows mount must target /workspace: {args:?}",
        );
    }

    #[test]
    fn container_cwd_is_identity_on_unix_and_workspace_relative_on_windows() {
        let mapping = PathMapping::new(std::path::Path::new("/proj"), None);
        #[cfg(unix)]
        assert_eq!(
            mapping.container_cwd(std::path::Path::new("/proj/api")),
            "/proj/api"
        );
        #[cfg(windows)]
        {
            let mapping = PathMapping::new(std::path::Path::new(r"C:\proj"), None);
            assert_eq!(
                mapping.container_cwd(std::path::Path::new(r"C:\proj\api")),
                "/workspace/api"
            );
        }
    }

    #[test]
    fn a_declared_workdir_overrides_the_computed_cwd() {
        let mapping = PathMapping::new(std::path::Path::new("/proj"), Some("/inside".to_string()));
        assert_eq!(
            mapping.container_cwd(std::path::Path::new("/proj/api")),
            "/inside"
        );
    }

    #[test]
    fn container_names_are_sanitized_and_unique() {
        let a = container_name("api:build");
        let b = container_name("api:build");
        assert!(a.starts_with("alba-api-build-"));
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        );
    }

    #[test]
    fn exec_args_carry_workdir_and_env() {
        let args = exec_args("cid", "/proj/api", &[("K".into(), "V".into())], "echo hi");
        assert_eq!(
            args,
            vec![
                "exec",
                "-w",
                "/proj/api",
                "-e",
                "K=V",
                "cid",
                "sh",
                "-c",
                "echo hi",
            ]
        );
    }
}
