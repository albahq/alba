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
//! itself is configured to talk to). The container is started with `--rm`,
//! which only reclaims it once it *stops* — since its main process is a
//! dormant `sleep` that never stops on its own, `--rm` is not a safety net
//! against a crashed Alba process. The `alba.beam=<beam>` label (see
//! `run_args`) is the only handle a later, out-of-process cleanup has for
//! finding and removing containers a crashed run left behind.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::process::Command;

use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor, Stream,
};

/// How long a cancelled command's whole teardown sequence — asking the
/// daemon to stop the container, removing it, and reaping the local
/// `docker` process — may take before escalating to a forceful kill of
/// that local process. This bounds the *entire* sequence, not just the
/// final wait: a slow or unreachable daemon must not be able to hold a
/// cancelled beam past the grace period this advertises.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long to wait for the stdout/stderr reader tasks to observe EOF
/// after a `docker exec` child has exited, before giving up on further
/// output and returning anyway. Mirrors `shell.rs`'s `DRAIN_PERIOD`: a
/// command that backgrounds a descendant inside the container could
/// otherwise hold the pipe open and block `execute()` forever waiting for
/// an EOF that never comes.
const DRAIN_PERIOD: Duration = Duration::from_secs(2);

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
    // The engine's rendered JSON (`executor_options` in `alba-engine`)
    // always sends `volumes` as an array, possibly empty, never absent and
    // never `null` — the DSL's `volumes ["h:/c"]` being optional only
    // means that array can be empty, not that the key itself can be
    // missing. `#[serde(default)]` is kept anyway as plain defensive
    // robustness against a hand-authored `options` value that does omit
    // it, not because Alba itself ever produces one.
    #[serde(default)]
    volumes: Vec<String>,
    // `workdir`, unlike `volumes`, genuinely can be `null` on the wire —
    // the DSL's `workdir` is truly optional, and the engine sends it as
    // JSON `null` rather than omitting the key — so this one needs to stay
    // an `Option`, not just a defaulted `Vec`.
    #[serde(default)]
    workdir: Option<String>,
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
                // Best-effort: killing the local `docker run -d` process
                // does not cancel container creation daemon-side, so this
                // can race and fail with "No such container" if the
                // daemon had not finished creating the container yet by
                // the time this `rm -f` reaches it. That failure is fine
                // on its own — it means the removal below simply lost a
                // race against a creation that has not landed *yet*, not
                // that there is nothing left to clean up: the daemon can
                // finish creating the container moments later regardless
                // of the local `docker run` process being killed, and
                // that container then runs the dormant `sleep 2147483647`
                // forever, which `--rm` never reclaims on its own (see the
                // module doc comment). What matters is that we wait for
                // the removal (bounded by STOP_GRACE) instead of
                // detaching it: a detached, unawaited `docker rm -f` risks
                // losing the race against the daemon actually finishing
                // container creation, which would orphan that container
                // rather than catch and remove it right after.
                let _ = tokio::time::timeout(STOP_GRACE, run_quiet(&["rm", "-f", &name])).await;
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
            mapping,
        }))
    }
}

/// The session opened for one beam: `container` (its docker-assigned id) is
/// the target of every `docker exec`/`stop`/`rm` this session issues. The
/// `--name` given at `run` time is not kept here — nothing reads it once
/// the id is known, and a stored-but-unread field is not worth carrying
/// against a future diagnostic that does not exist yet.
#[derive(Debug)]
struct DockerSession {
    container: String,
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

        let status_result: Result<ExitStatus, ExecError> = tokio::select! {
            result = child.wait() => result.map_err(|error| ExecError {
                message: format!("failed to wait for `docker exec`: {error}"),
            }),
            _ = ctx.cancel.cancelled() => {
                // Ask the daemon to stop the container, then make sure the
                // local `docker exec` process is gone. The whole sequence
                // — stop, rm, and reaping the child — is bounded by
                // STOP_GRACE so a slow or unreachable daemon cannot hold
                // the beam past the grace period it advertises; only the
                // fallback kill is unbounded, and it targets our own local
                // process, which cannot itself hang on the daemon.
                let teardown = async {
                    run_quiet(&["stop", "--time", &STOP_GRACE.as_secs().to_string(), &self.container]).await;
                    run_quiet(&["rm", "-f", &self.container]).await;
                    let _ = child.wait().await;
                };
                if tokio::time::timeout(STOP_GRACE, teardown).await.is_err() {
                    let _ = child.kill().await;
                }
                child.wait().await.map_err(|error| ExecError {
                    message: format!("failed to wait for cancelled `docker exec`: {error}"),
                })
            }
        };

        // On error, nobody is going to read `ctx.output` after `execute`
        // returns; abort the reader tasks instead of leaving them
        // detached and still writing to a channel side nothing drains.
        let status = match status_result {
            Ok(status) => status,
            Err(error) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(error);
            }
        };

        // The child has exited, so its pipes are closing/closed; let the
        // reader tasks drain what's left, bounded by DRAIN_PERIOD — see
        // its doc comment, mirroring `shell.rs`'s identical reasoning: an
        // unconditional join could hang forever on a surviving descendant
        // inside the container still holding a pipe open.
        let drain = async {
            let _ = tokio::join!(&mut stdout_task, &mut stderr_task);
        };
        if tokio::time::timeout(DRAIN_PERIOD, drain).await.is_err() {
            stdout_task.abort();
            stderr_task.abort();
        }

        Ok(ExecResult {
            exit_code: status.code().unwrap_or(-1),
        })
    }

    async fn close(self: Box<Self>) -> Result<(), ExecError> {
        // Piped (not null) stderr: a genuine failure here reaches the
        // user (the engine downgrades it to a notice, but does not
        // discard it), so its diagnosis should not be reduced to a bare
        // exit code.
        let output = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(docker_spawn_error)?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("No such container") {
            // A cancelled `execute` already stopped and removed this
            // container (see its teardown sequence); `close` still runs
            // unconditionally afterwards, so this is the expected,
            // truthful outcome, not a failure the user needs to see.
            return Ok(());
        }
        let code = output.status.code().unwrap_or(-1);
        Err(ExecError {
            message: format!("docker rm exited with code {code}: {}", stderr.trim()),
        })
    }

    async fn kill(self: Box<Self>) {
        // The same removal `close` performs — `docker rm -f` reaches
        // everything running inside the container, which is the whole
        // process group concern `kill`'s contract cares about — but
        // fire-and-forget: a caller reaching for `kill` instead of `close`
        // has no use for the diagnostic `close` returns on failure.
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
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
        // `workdir` is genuinely optional on the wire and can be sent as
        // an explicit JSON `null` (see the struct's own doc comment);
        // `volumes` is exercised as fully absent instead, since the
        // engine never actually sends it as `null` — absence is what its
        // `#[serde(default)]` is really defending against.
        let config: DockerConfig =
            serde_json::from_value(serde_json::json!({"image": "alpine:3", "workdir": null}))
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
        #[cfg(unix)]
        {
            let mapping = PathMapping::new(std::path::Path::new("/proj"), None);
            assert_eq!(
                mapping.container_cwd(std::path::Path::new("/proj/api")),
                "/proj/api"
            );
        }
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
