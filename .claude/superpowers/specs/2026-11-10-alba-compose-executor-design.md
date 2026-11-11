# Alba: Compose Executor

Date: 2026-11-10 Status: approved

## Overview

A new built-in executor, `executor compose { service "php" }`, runs a beam's commands inside a one-off
container of a service declared in a Compose file. Everything Compose already knows how to describe (image,
build, mounts, user, environment, networks, profiles, dependencies) stays in `compose.yaml`; the block names
the service and nothing else.

It follows the [docker executor and plugin protocol design](2026-08-06-alba-docker-plugins-design.md) and
reuses its session contract, with one addition: an `Executor::shutdown` method (see
[Contract change](#contract-change)).

## Why

Alba 0.4.0 added `build`, `dockerfile`, `user`, and relative container paths in `volumes` to the docker
executor. Each of them reimplements a piece of Compose. The docker executor stops growing here: a project that
already describes its containers in `compose.yaml` points Alba at that description instead of repeating it in
every Beamfile.

The motivating project is a monorepo (called "the reference project" below) whose `compose.yaml` sits at the
repository root and declares:

- a `php` service: built from `docker/php`, build arguments carrying the developer's uid and gid, profile
  `tools`, `working_dir: /app`, the repository bind-mounted on `/app`;
- two Node services running documentation sites: each bind-mounts `apps/<site>` on `/app`, keeps
  `node_modules` in a named volume the image prepared with the right owner, publishes a port, and has a
  `command` that starts a development server.

Its modules live in `modules/<name>/`, each with a Beamfile that runs both imported from the root Beamfile and
on its own, from the module directory. Today every module Beamfile carries an `image` beam
(`docker compose build --quiet php`), a `let` holding the image name, and
`executor docker { image "{image_php}" }` on six beams. The Node tools cannot be beams at all.

## Goals

1. A beam targets a Compose service by name; the service definition is the single source of truth.
2. The Compose file is found whether the beam runs from the root project or from a Beamfile several
   directories below it.
3. A beam's host working directory is translated to its path inside the container from the service's own
   mounts.
4. A beam replaces the service's command without starting the service itself and without publishing its ports.
5. Nothing the executor does overrides the service's user: file ownership is whatever `compose.yaml` arranged.
6. The service image is built when missing or stale, with no dedicated beam, at negligible cost when cached.
7. Beams targeting the same service run in parallel, one container each.
8. A service behind a profile can be targeted without activating the profile by hand.
9. A beam never stops, restarts, recreates, or removes anything it did not start: services brought up by
   `docker compose up`, their containers, their volumes.
10. Dependencies (`depends_on`) a beam needs are started by Alba when absent and put back in their previous
    state when the run ends, or when the watch or TUI session ends.

## Non-goals

- Any option that duplicates what Compose describes: `user`, `volumes`, `workdir`, `env`, `profiles`,
  `build`. If a need arises, it is solved in `compose.yaml`.
- Publishing the service's ports (`--service-ports`).
- Putting the content of the Compose file or of Dockerfiles in the cache fingerprint (see
  [Cache](#cache)).
- Any change to the docker executor's options or to the plugin protocol.
- An API client: the executor shells out to `docker compose`, as the docker executor shells out to `docker`.

## Anti-goals

The feature is a failure, even if beams pass, when a developer who has `docker compose up` running loses a
container, a volume, or a database state because an Alba beam ran. `docker compose down`, `--remove-orphans`,
`--volumes`/`-v` on a removal, and `--force-recreate` are never issued.

## Key decisions

| Topic             | Decision                                                                                          |
| ----------------- | ------------------------------------------------------------------------------------------------- |
| Name              | `compose`, a built-in executor name                                                               |
| Options           | `service` (required), `file` (optional)                                                           |
| Container         | One per beam, created by `docker compose run`, commands through `docker exec`                     |
| Keep-alive        | The container's process is `sh` reading a stdin pipe Alba holds open                              |
| Working directory | Derived from the service's bind mounts in `docker compose config`                                 |
| Image             | `docker compose build <service>` at every session open, when the service declares a build         |
| Dependencies      | Started by Alba with `up -d --no-recreate`, never by `run`; released when the executor shuts down |
| Cache fingerprint | The executor name and the text of its options, like the docker executor                           |
| Executor contract | `Executor::shutdown` added, a no-op by default; `ExecSession` and the plugin protocol unchanged   |

## DSL

```text
beam test {
  executor compose { service "php" }
  run "vendor/bin/phpunit"
}

beam build {
  executor compose { service "help-center" file "../../compose.tools.yaml" }
  run "pnpm build"
}
```

- `service`: required, non-empty string. The Compose service the beam's commands run in.
- `file`: optional, non-empty string. Path to the Compose file, relative to the directory of the Beamfile that
  declares the beam (the same base as the docker executor's `build`). When absent, Compose's own lookup
  applies: the beam's directory, then each parent directory, and the `COMPOSE_FILE` variable.
- Interpolation is allowed in both, as in the docker block.
- Any other option is an evaluation error listing the known options, with the same wording as the docker
  block's.

`compose` joins `shell`, `system_shell`, and `docker` as a reserved executor name. A plugin binary named
`alba-executor-compose` is no longer reachable; the changelog says so.

`alba check` validates the block's shape only. It never runs `docker`, so a Beamfile validates on a machine
without Docker, and an unknown service name is a run-time failure.

## Model and wiring

- `alba-core`: `ExecutorKind::Compose { service: String, file: Option<String> }`, built in `eval.rs` next to
  `build_docker`.
- `alba-engine`: `executor_options` serializes it as `{"service": ..., "file": ...}`; the cache label follows
  the docker label's pattern; `Executors` gains a `compose` slot and `for_beam` dispatches to it.
- `alba-executors`: a new `compose` module exposing `ComposeExecutor`. It needs no project root: the beam's
  directory and the service's mounts are enough.
- `alba`: `run.rs` builds one `ComposeExecutor` next to the docker one. Like every executor, that instance
  lives as long as the command: a single run for `alba run`, every run of a watch or TUI session. It is where
  the dependencies Alba started are remembered.

## Contract change

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    async fn open(&self, beam: BeamContext) -> Result<Box<dyn ExecSession>, ExecError>;
    /// Releases whatever the executor holds across sessions. Default: nothing.
    async fn shutdown(&self) -> Result<(), ExecError> { Ok(()) }
}
```

- The engine calls `shutdown` once on every executor it was given, when the command is about to end: after
  the last beam of a plain run, and when a watch or TUI session ends, never between two runs of such a
  session. It is called for every outcome, cancellation and failed runs included, after every session is
  closed.
- A `shutdown` error is reported to the user and makes the command exit with a failure, without hiding a beam
  failure that came first. The exact rendering is left to the builder.
- The existing executors, plugin executors included, keep the default. `FakeExecutor` records the call so the
  engine's tests can assert when it happens. `ExecSession` and the plugin protocol are untouched.

Why the contract changes: releasing dependencies when the last session using them closes needs no new method,
but in watch mode it removes and restarts a database at every saved file. Only the engine knows when a watch
session is over.

Code the docker module already has and the compose module needs (the `docker exec` argument builder, line
streaming, the stop-then-remove cancellation ladder, the container naming scheme, the `docker` spawn error)
is shared between the two modules rather than copied. How it is factored is left to the builder.

## Session lifecycle

Every `docker compose` invocation runs with the beam's directory as its working directory and, when `file` is
set, `-f <absolute path of file>`.

### Open

1. **Read the service.** `docker compose config --format json` gives the project name and, for the target
   service: its bind mounts (absolute host `source`, container `target`), its `depends_on`, and whether it
   declares a `build`. A failure here (no Compose file found, invalid file, unknown service) fails the beam
   with Compose's own message.
2. **Build.** When the service declares a `build`, run `docker compose build <service>`. Like the docker
   executor's own build, it is quiet when it succeeds and puts the build log on the beam's output when it
   fails. No deduplication across parallel beams: BuildKit already shares the work.
3. **Dependencies.** See [Dependencies](#dependencies).
4. **Start the container.**

   ```text
   docker compose run --rm -T --no-deps --name <name> --label alba.beam=<beam> <service> sh
   ```

   with stdin piped from Alba and kept open, stdout and stderr relayed on the beam's output. `<name>` follows
   the docker executor's scheme (`alba-` prefix, beam name, unique suffix), which is what lets two beams
   target the same service at once. No `--user`, no `-e`, no `-v`, no `-w`, no `--service-ports`.
5. **Wait until the container accepts `docker exec`.** How readiness is detected is left to the builder. The
   wait ends in failure as soon as the `docker compose run` process exits, and is interrupted by the beam's
   cancellation token.

Documented prerequisites: the service's image provides `sh`, and an `entrypoint`, if the service has one, ends
up executing the command it is given.

### Execute

```text
docker exec -w <container cwd> -e K=V ... <name> sh -c "<command>"
```

Identical to the docker executor, including output streaming and exit codes. The command runs as the
container's own user, so ownership of written files is the service's business.

`<container cwd>` comes from [Working directory](#working-directory).

### Close

1. Close the stdin pipe: `sh` exits and `--rm` removes the container. Wait for the `docker compose run`
   process, bounded by the same grace period the docker executor uses.
2. `docker rm -f <name>`, unconditionally and best effort, in case step 1 did not finish in time.

`kill` does step 2 immediately. Neither touches the dependencies.

### Cancellation

- During open: kill the in-flight `docker compose` process; if it was the `run`, `docker rm -f <name>` and
  wait for it, bounded, as the docker executor does for `docker run`.
- During execute: the docker executor's ladder, applied to `<name>`: `docker stop`, then removal, bounded by
  the same grace period.

Dependencies started before a cancellation are released by `shutdown`, like any other.

### After a `kill -9` of Alba

The stdin pipe closes with the process, `sh` exits, and `--rm` removes the beam's container. Nothing sleeps
forever. Dependencies Alba had started stay up: nothing is left alive to stop them. The documentation gives
the manual cleanup.

## Working directory

For each command, the executor takes the command's host `cwd` and looks, among the service's bind mounts, for
the one whose `source` is the longest path prefix of it. The container working directory is that mount's
`target` followed by the remainder of the path.

In the reference project:

| Service       | Mount                         | Host cwd              | Container cwd              |
| ------------- | ----------------------------- | --------------------- | -------------------------- |
| `php`         | repository root on `/app`     | `modules/facturation` | `/app/modules/facturation` |
| `help-center` | `apps/help-center` on `/app`  | `apps/help-center`    | `/app`                     |

- Both sides are compared in canonical form: `docker compose config` reports resolved paths (`/private/tmp/…`
  where the shell says `/tmp/…` on macOS).
- Only bind mounts count; a named volume has no host path.
- When no bind mount covers the `cwd`, the command fails with an error naming the `cwd`, the service, and the
  bind mounts the service does have. There is no fallback to the service's `working_dir`: running a tool in
  the wrong directory silently is worse than failing.
- The rule is the same on Windows, where it replaces the docker executor's fixed `/workspace` convention. Path
  comparison details per platform are left to the builder.

## Dependencies

`docker compose run` starts a service's dependencies itself, and recreates one that is already running when
its definition in the Compose file has changed since it was started. That would restart a developer's
database under them, so the executor passes `--no-deps` and manages dependencies on its own.

At open, for the transitive closure of the target service's `depends_on`:

1. Record each dependency's current state from `docker compose ps -a`: absent, stopped, or running.
2. `docker compose up -d --no-recreate <direct dependencies>`. A running dependency is left exactly as it is,
   diverged definition included; an absent or stopped one is started. The command is quiet when it succeeds
   and its output goes to the beam's output when it fails.
3. `depends_on` conditions (`service_healthy`, `service_completed_successfully`) must be honoured as
   `docker compose run` would honour them before the beam's container starts. Measured: `up --wait` handles
   `service_healthy` and `service_started`, not `service_completed_successfully`, which goes through
   `docker wait` on the one-shot's containers (found by their Compose labels). `docker compose wait` cannot
   serve: before Compose 5.6 it does not see a container that already exited.

The `ComposeExecutor` keeps, per Compose project (keyed by the project name `config` reports, with the
directory and `file` needed to address it again), the dependencies **Alba started** and the state each had
before: absent or stopped. Steps 1 and 2 and that bookkeeping happen under a per-project lock, so two beams
opening at once cannot both believe they started the same service.

- A dependency that was already running and is not recorded is not Alba's: it is never recorded, never
  stopped.
- A dependency already recorded stays recorded with its original state.

At `shutdown`, every recorded dependency goes back to its previous state and the record is emptied:

- it was absent: `docker compose rm -s -f <service>` (stop and remove the container, never its volumes);
- it was stopped: `docker compose stop <service>`.

Every project is attempted even when one fails; the failures are reported together.

Accepted limits, to be documented:

- A dependency Alba started stays up until the command ends: for the whole watch or TUI session.
- Alba cannot tell that a dependency it started has since been adopted: if the developer runs
  `docker compose up` while Alba holds it, Compose reuses that container and Alba still removes it at
  `shutdown`. No volume is removed, so no data is lost, and `docker compose up` brings it back.
- Two Alba processes running at once on the same Compose project do not share their records: one may stop a
  dependency the other still uses.
- After a `kill -9` of Alba, started dependencies stay up.
- `docker compose run` creates the project's default network when it does not exist, and leaves it.

## Cache

The fingerprint contains the executor name and the rendered text of `service` and `file`, nothing else, as
the docker executor does with its options. A beam whose result depends on the Compose file or on a Dockerfile
lists them in `inputs`, exactly as the reference project already lists `../../docker/php/Dockerfile`.

Reason: the fingerprint is computed without touching Docker, and no reading of the Compose file can know what
a build context copies into the image.

## Errors

All are beam failures with the cause stated, never panics:

- `docker` missing from the `PATH`, or `docker compose` not available: the message names the missing piece.
- No Compose file found, invalid Compose file, unknown service: Compose's message, relayed.
- Build failure: the build log is on the beam's output.
- A dependency that fails to start or to become healthy.
- A `cwd` no bind mount covers.
- Invalid options reaching `open` (hand-authored JSON): rejected like the docker executor's.

## Testing

The existing pattern applies:

- `alba-core`: unit tests on evaluation: `service` carried, `file` carried, missing or empty `service`
  rejected, empty `file` rejected, unknown option rejected with the list of known ones, interpolation.
- `alba-engine`: the options JSON and the cache label (changing `service` or `file` invalidates the entry);
  with `FakeExecutor`, `shutdown` is called once after a plain run, for a successful, a failed, and a
  cancelled run, and once at the end of a watch session, not between its runs.
- `alba-executors`: unit tests on what is pure: argument lists, the host-to-container path mapping (longest
  prefix, no covering mount, named volumes ignored), the dependency closure, and the record of started
  dependencies.
- `alba-executors/tests/compose.rs` and `alba/tests/cli_compose.rs`: against a real daemon,
  `#[ignore = "requires a running docker daemon"]`, each with a throwaway Compose project of `alpine:3`
  services under a unique project name. They cover at least:
  - a command runs in the service, in the translated working directory, with the beam's `env`;
  - state left by one command of a beam is visible to the next;
  - the Compose file is found from a Beamfile two directories below it;
  - a service behind a profile is reachable;
  - two beams on the same service run concurrently;
  - a service with a long-running `command` and published ports runs the beam's command instead, with no port
    published;
  - a running service of the same project is untouched by a beam (same container id before and after), also
    when its definition has diverged;
  - an absent dependency is started, stays up after the session closes, and is removed at `shutdown`; a
    stopped one is started, then stopped at `shutdown`; a running one is left alone;
  - a second session reuses a dependency the first one started;
  - cancellation during a command removes the container; the following `shutdown` releases the dependencies;
  - closing the stdin pipe from outside (the stand-in for a killed Alba) removes the container.

The linux CI job that already runs the docker tests runs these.

## Documentation

- `README.md`: a "Compose executor" section after "Docker executor": the block, the lookup of the Compose
  file, the working directory rule, the dependency lifecycle and its limits, the prerequisites, the manual
  cleanup. The docker section points to it for projects that already have a Compose file.
- `CHANGELOG.md`: not edited by hand; the entry comes from the feature commit, whose body states the reserved
  name.
- `alba-agent-plugin/skills/using-alba/SKILL.md` and `references/beamfile.md`: the new executor.

## Success criteria

In the reference project, with no change to its `compose.yaml` or Dockerfiles:

- the module Beamfile drops its `image` beam and its `let`, every tool beam declares
  `executor compose { service "php" }`, and `alba run check` passes both from the repository root and from
  `modules/facturation`;
- `lint`, `analyse`, and `test` run in parallel;
- `vendor/` and the coverage report belong to the person running Alba on Linux;
- a Beamfile in `apps/help-center` runs `pnpm install`, `pnpm typecheck`, and `pnpm build` in the
  `help-center` service while `docker compose up` serves the same site, and the running containers keep their
  ids.

And in Alba: the docker executor's and the plugin protocol's test suites pass unchanged.

## Findings from the throwaway spike

Measured with Docker Compose v5.6.1 on Docker Desktop 29.9.2 (macOS):

- A container started by `docker compose run --rm -T --name … <service> sh` with a held stdin pipe accepts
  `docker exec`, keeps state between two `exec` calls, and reports their exit codes. Killing the process
  holding the pipe removed the container in about 600 ms.
- `docker compose build <service>`, fully cached: 0.25 s alone, 0.5 s for three in parallel.
- `docker compose config --format json`, run two directories below the Compose file, finds it and reports
  absolute bind sources.
- Targeting a service activates its profile; `run` replaces the service's command and publishes no port;
  `--label` is applied; two `run` containers of one service coexist.
- `docker compose run` recreated a running dependency whose definition had changed.
  `docker compose up -d --no-recreate` left it untouched and started an absent one; `run --no-deps` then
  touched nothing. `docker compose rm -s -f <service>` took about 3 s on a container ignoring `SIGTERM`.

Not measured: anything on Linux or Windows.

## Deferred

Nothing from the stated needs. Two things are deliberately outside this design:

- The Compose file's content in the cache fingerprint (see [Cache](#cache)).
- Giving the docker executor the same stdin-held keep-alive, which would end its orphaned
  `sleep 2147483647` containers after a crash. It is a robustness fix independent from this feature and
  belongs in its own change.
