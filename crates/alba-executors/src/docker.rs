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
use std::process::Stdio;

use tokio::process::Command;

use crate::container::{self, STOP_GRACE, container_name, run_quiet, spawn_error};
use crate::shell::stream_lines;
use crate::{
    BeamContext, CommandSpec, ExecContext, ExecError, ExecResult, ExecSession, Executor, Stream,
};

/// What the docker executor needs from the host, finishing the message of a
/// failed `docker` spawn.
const DOCKER_REQUIREMENT: &str = "the docker executor requires Docker on the PATH";

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
    // The image the container starts from. Optional because a `build`
    // context can stand in for it; the loader guarantees at least one of
    // the two, and `open` rejects a hand-authored `options` with neither.
    #[serde(default)]
    image: Option<String>,
    // A build context: a directory holding a `Dockerfile`, relative to the
    // beam's own directory. When set, the container starts from the image
    // built out of it (see `build_image`), tagged `image` when both are
    // declared.
    #[serde(default)]
    build: Option<String>,
    // The file `build` is built from, relative to that context, when it is
    // not the context's own `Dockerfile`. Meaningless without `build`,
    // which the loader enforces.
    #[serde(default)]
    dockerfile: Option<String>,
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
    // Who the beam's commands run as: anything `docker run --user`
    // accepts, or `host` (see `HOST_USER`). Absent or `null` leaves the
    // choice to the image.
    #[serde(default)]
    user: Option<String>,
}

/// The `user` value that stands for whoever runs Alba, rather than for a
/// user of the image: see [`container_user`].
const HOST_USER: &str = "host";

#[async_trait::async_trait]
impl Executor for DockerExecutor {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError> {
        let config: DockerConfig =
            serde_json::from_value(beam.options.clone()).map_err(|error| ExecError {
                message: format!("invalid docker options: {error}"),
            })?;
        let image = match (&config.build, &config.image) {
            (Some(context), tag) => {
                let build = ImageBuild {
                    context: beam.dir.join(context),
                    declared: context,
                    dockerfile: config.dockerfile.as_deref(),
                    tag: tag.as_deref(),
                };
                build_image(&build, &beam).await?
            }
            (None, Some(image)) => image.clone(),
            (None, None) => {
                return Err(ExecError {
                    message: "invalid docker options: neither `image` nor `build` is set".into(),
                });
            }
        };
        let name = container_name(&beam.beam);
        let mapping = PathMapping::new(&self.project_root, config.workdir.clone());
        let config = DockerConfig {
            volumes: config
                .volumes
                .iter()
                .map(|volume| mapping.container_volume(volume, &beam.dir))
                .collect(),
            ..config
        };

        let args = run_args(&config, &image, &self.project_root, &name, &beam.beam);
        let mut child = Command::new("docker")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| spawn_error(error, DOCKER_REQUIREMENT))?;

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
                message: format!("docker run exited with code {code} for image `{image}`"),
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
        container::exec(&self.container, &cwd, &cmd, ctx, DOCKER_REQUIREMENT).await
    }

    async fn close(self: Box<Self>) -> Result<(), ExecError> {
        container::remove(&self.container, DOCKER_REQUIREMENT).await
    }

    async fn kill(self: Box<Self>) {
        container::force_remove(&self.container).await;
    }
}

/// Builds the image a beam's container starts from out of `context` (a
/// directory holding a `Dockerfile`), and returns that image's id.
///
/// The build runs before every session rather than once: docker's own
/// layer cache is what makes the repeat cheap, and it is the only party
/// that knows whether the context changed. `--quiet` keeps that repeat
/// silent — on success docker prints the image id alone, on failure it
/// still prints the whole build log — so one line on the beam's output
/// says what is happening while a first, slow build runs.
///
/// The container is started from the returned id, never from `tag`: the
/// tag only names the image for whoever wants to reuse it outside Alba,
/// and the id cannot be repointed between the build and the run.
async fn build_image(build: &ImageBuild<'_>, beam: &BeamContext) -> Result<String, ExecError> {
    let declared = build.declared;
    let _ = beam.output.send(crate::OutputLine {
        stream: Stream::Stderr,
        text: format!("building the image from `{declared}`"),
    });

    let mut child = Command::new("docker")
        .args(build_args(build))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_error(error, DOCKER_REQUIREMENT))?;

    let mut stdout = child
        .stdout
        .take()
        .expect("child spawned with piped stdout");
    let stderr = child
        .stderr
        .take()
        .expect("child spawned with piped stderr");
    let stdout_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stdout, &mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(stream_lines(stderr, Stream::Stderr, beam.output.clone()));

    let status = tokio::select! {
        result = child.wait() => result.map_err(|error| ExecError {
            message: format!("failed to wait for `docker build`: {error}"),
        })?,
        _ = beam.cancel.cancelled() => {
            // Killing the local `docker build` process is what cancels the
            // build daemon-side: unlike `docker run -d`, it stays attached
            // to the build for its whole duration.
            let _ = child.start_kill();
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(ExecError {
                message: "image build cancelled".into(),
            });
        }
    };

    let image = stdout_task.await.unwrap_or_default();
    let _ = stderr_task.await;

    if !status.success() {
        let code = status.code().unwrap_or(-1);
        return Err(ExecError {
            message: format!("docker build exited with code {code} for context `{declared}`"),
        });
    }

    Ok(image.trim().to_string())
}

/// What one `docker build` needs: see [`build_image`].
struct ImageBuild<'a> {
    /// The build context, as a host path.
    context: PathBuf,
    /// The same context as the Beamfile spells it, for messages.
    declared: &'a str,
    /// The file to build from, relative to `context`, when it is not the
    /// context's own `Dockerfile`.
    dockerfile: Option<&'a str>,
    /// The name the built image is tagged with, if any.
    tag: Option<&'a str>,
}

/// Builds the argument vector for `docker build`: see [`build_image`].
///
/// `docker build --file` resolves a relative path against the directory
/// `docker` runs in, not against the context; the declared `dockerfile` is
/// relative to the context, so it is joined to it here.
fn build_args(build: &ImageBuild<'_>) -> Vec<String> {
    let mut args = vec!["build".to_string(), "--quiet".to_string()];
    if let Some(tag) = build.tag {
        args.push("--tag".to_string());
        args.push(tag.to_string());
    }
    if let Some(dockerfile) = build.dockerfile {
        args.push("--file".to_string());
        args.push(build.context.join(dockerfile).display().to_string());
    }
    args.push(build.context.display().to_string());
    args
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
        match &self.workdir {
            Some(workdir) => workdir.clone(),
            None => self.container_path(cwd),
        }
    }

    /// Rewrites a declared volume whose container part is relative to the
    /// beam's directory (`deps:./node_modules`) into the absolute container
    /// path docker needs: `beam_dir` as the container sees it, joined with
    /// that relative part. Any other volume is returned as declared. The
    /// loader has already checked the entry's shape, so only its last
    /// colon splits (a windows host path keeps its drive colon).
    fn container_volume(&self, volume: &str, beam_dir: &Path) -> String {
        match volume.rsplit_once(":./") {
            Some((source, relative)) => {
                let base = self.container_path(beam_dir);
                let target = format!("{}/{relative}", base.trim_end_matches('/'));
                format!("{source}:{}", target.trim_end_matches('/'))
            }
            None => volume.to_string(),
        }
    }

    /// Where a host path under the project root is found inside the
    /// container, regardless of any declared `workdir`.
    fn container_path(&self, host: &Path) -> String {
        #[cfg(unix)]
        {
            host.to_string_lossy().into_owned()
        }

        #[cfg(windows)]
        {
            match host.strip_prefix(&self.project_root) {
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
/// comment) from `image` — the declared one, or the id of the one just
/// built. The project root is bind-mounted so the container sees the same
/// files the beam's commands would see running on the host; the beam's
/// declared volumes are appended verbatim after it.
fn run_args(
    config: &DockerConfig,
    image: &str,
    project_root: &Path,
    name: &str,
    beam: &str,
) -> Vec<String> {
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

    if let Some(user) = config.user.as_deref().and_then(container_user) {
        args.push("--user".to_string());
        args.push(user);
    }

    args.push(image.to_string());
    args.push("sh".to_string());
    args.push("-c".to_string());
    args.push("sleep 2147483647".to_string());
    args
}

/// Resolves a declared `user` to what `docker run --user` takes. Every
/// value is passed through as declared but [`HOST_USER`], which becomes the
/// `uid:gid` of the Alba process itself: files a beam writes into the
/// project mount then belong to the person running Alba, not to whatever
/// user the image defaults to (usually root).
///
/// Windows has no uid to hand over, and Docker Desktop already maps the
/// ownership of bind-mounted files there, so `host` leaves the image's own
/// user in place.
fn container_user(declared: &str) -> Option<String> {
    if declared != HOST_USER {
        return Some(declared.to_string());
    }

    #[cfg(unix)]
    {
        Some(format!(
            "{}:{}",
            nix::unistd::getuid(),
            nix::unistd::getgid()
        ))
    }
    #[cfg(not(unix))]
    {
        None
    }
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
                image: Some("alpine:3".into()),
                build: None,
                dockerfile: None,
                volumes: vec![],
                workdir: None,
                user: None,
            }
        );
    }

    #[test]
    fn run_args_mount_the_project_and_keep_the_container_dormant() {
        let config = DockerConfig {
            image: Some("alpine:3".into()),
            build: None,
            dockerfile: None,
            volumes: vec!["h:/c".into()],
            workdir: None,
            user: None,
        };
        let args = run_args(
            &config,
            "alpine:3",
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
    fn a_relative_volume_resolves_under_the_beams_directory_in_the_container() {
        // A declared `workdir` moves where commands run, not where the
        // beam's directory is mounted: it must not move the volume.
        #[cfg(unix)]
        let (mapping, beam_dir, expected) = (
            PathMapping::new(Path::new("/proj"), Some("/inside".to_string())),
            Path::new("/proj/api"),
            "deps:/proj/api/node_modules",
        );
        #[cfg(windows)]
        let (mapping, beam_dir, expected) = (
            PathMapping::new(Path::new(r"C:\proj"), Some("/inside".to_string())),
            Path::new(r"C:\proj\api"),
            "deps:/workspace/api/node_modules",
        );

        assert_eq!(
            mapping.container_volume("deps:./node_modules", beam_dir),
            expected
        );
        assert_eq!(
            mapping.container_volume("/host/cache:/cache", beam_dir),
            "/host/cache:/cache"
        );
    }

    #[test]
    fn build_args_find_the_declared_dockerfile_inside_the_context() {
        let build = ImageBuild {
            context: PathBuf::from("ctx"),
            declared: "ctx",
            dockerfile: Some("dev.Dockerfile"),
            tag: Some("app"),
        };
        let dockerfile = Path::new("ctx").join("dev.Dockerfile");
        assert_eq!(
            build_args(&build),
            vec![
                "build",
                "--quiet",
                "--tag",
                "app",
                "--file",
                &dockerfile.display().to_string(),
                "ctx",
            ]
        );
    }

    #[test]
    fn a_declared_workdir_overrides_the_computed_cwd() {
        let mapping = PathMapping::new(std::path::Path::new("/proj"), Some("/inside".to_string()));
        assert_eq!(
            mapping.container_cwd(std::path::Path::new("/proj/api")),
            "/inside"
        );
    }
}
