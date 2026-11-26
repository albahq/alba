# Beamfile DSL

Brace-delimited, whitespace-insensitive. A field may appear at most once
per block; lists accept a trailing comma.

## File-level declarations

```
version "1"                          # optional
import "api/Beamfile" as api         # beams reachable as api:<name>
let profile = env("PROFILE", "debug")
default check                        # the beam a bare `alba` runs
git { base 'origin/develop' }        # the reference git.changes() diffs from

beam <name>[(param, ...)] { ... }
hook <git-hook-name> { beam <name> }
```

Only the root Beamfile's `default` and `hook` declarations count; an
imported file's are ignored. A `git` block is allowed once, in the root
Beamfile only: anywhere else it is a load error. Namespaces chain: a beam
imported through two levels is `api:db:migrate`.

In `needs` only (alba 0.3.0 and later), `*` stands for any one import
alias: `needs [*:test]` depends on the `test` beam of every direct import
that declares one, in import order. Each `*` matches exactly one level
(`*:*:test` reaches `api:db:test`), the beam name is always spelled out
(`api:*` is rejected), and a pattern matching no beam is a load error.

## Beam fields

| Field | Value | Notes |
| --- | --- | --- |
| `description` | string template | Shown in listings. |
| `needs` | `[beam, api:beam, *:beam, ...]` | Prerequisites; all must succeed first. `*` stands for any one import alias. |
| `inputs` | `["src/**/*.rs", ...]` | Globs, relative to the declaring Beamfile's directory; leading `..` components climb out of it (`"../../Dockerfile"`), while `.` or `..` past the start is a load error. A glob skips `.gitignore`d files; a literal path is always an input. Turns caching on, and drives `--watch`, `--affected` and `git.changes()`. |
| `env_inputs` | `["NODE_ENV", "PATH"]` | Ambient variables whose values (or absence) join the cache fingerprint; the inherited environment is otherwise ignored. Each entry is a variable name (letters, digits, `_`, no leading digit). Requires `inputs`. Needs alba 0.6.0 or later. |
| `secrets` | `["NPM_TOKEN"]` | Variables (from the beam's `env`, else the environment Alba was started in) whose values are replaced by `***` in the output Alba shows and stores: live, JSON and cached. Masking is line by line, and does not cover what a command writes to a file or sends elsewhere. Needs alba 0.6.0 or later. |
| `outputs` | `["target/release/alba"]` | Same base directory as `inputs`. The beam reruns when an output is missing, and since alba 0.6.0 also when one was rewritten (size or modification time) or removed from a glob's matches or a directory. |
| `run` | `"cmd"` or `["cmd1", "cmd2"]` | String templates. Commands run in order on the same executor. |
| `env` | `{ NAME = "value", OTHER = param }` | Values are string templates or a bare `let` / parameter name. |
| `cwd` | string template | Working directory for the commands. |
| `executor` | `system_shell`, `docker { ... }`, `compose { ... }`, or `<plugin> { ... }` | Omitted means the embedded shell. |
| `allow_failure` | `true` | The beam may fail without failing its dependents. |

Example:

```
beam deploy(target) {
  description "Deploy to {target}"
  needs [build, api:migrate]
  inputs ["dist/**"]
  env { DEPLOY_TARGET = target }
  executor docker {
    image "deployer:latest"
    volumes ["/host/cache:/cache"]
    workdir "/srv"
    user "host"
  }
  run ["./scripts/preflight.sh", "./scripts/deploy.sh {target}"]
}
```

Parameters bind positionally: `alba run deploy prod`. A parameterized beam
is not cached, is skipped by a bare `--affected` run, and is not statically
shell-checked.

## Expressions

Anything inside `{...}` in a string is an expression. Inside the braces,
string literals use **single** quotes (`"{env('VAR')}"`); `{{` and `}}`
give literal braces.

- `let name = <expr>` at file level.
- `env("NAME", "fallback")` reads an environment variable. Without a
  fallback, `env("NAME")` is allowed only in `run` and `env` values, where
  it resolves at run time; anywhere resolved at load time (`let`,
  `description`, `inputs`, `outputs`, `cwd`, executor options) it is a load
  error.
- `git.branch`, `git.sha`, `git.short_sha` (strings), `git.dirty` (bool).
  Usable in every expression position. `git` is a reserved name.
- Operators, low to high precedence: `||`, `&&`, `==` / `!=`, `+`.
- `if cond then a else b`, for example `{if git.dirty then '-dirty' else ''}`.
- `match subject { 'glob' => value  'a' | 'b' => value  _ => value }`: the
  first arm whose glob matches the string subject wins (`*` never crosses
  `/`); `_` is required and comes last. Patterns are literals, and every arm
  is validated at load time, taken or not. `match` is reserved. Usable
  inside an interpolation:
  `"{match git.branch { 'main' => 'prod'  _ => 'dev' }}"`.
- `true`, `false`, and bare identifiers referring to `let` bindings or beam
  parameters.

## The `git` block and incremental beams

```
git {
  base env('GITHUB_BASE_REF', match git.branch {
    'hotfix/*' | 'master' => 'origin/master'
    _                     => 'origin/develop'
  })
}

beam phpcs {
  inputs ["src/**/*.php", "phpcs.xml"]
  run "vendor/bin/phpcs {git.changes('src/**/*.php')}"
}

beam phpstan {
  inputs ["src/**/*.php", "phpstan.neon"]
  run "vendor/bin/phpstan analyse {git.changes('src/**/*.php').dirs('src/*')}"
}
```

- `base` is evaluated at load time (`env()` needs a default) and may read
  `git.*` and every `let`. `''` means no reference. `base` is the only key.
  When git cannot answer while evaluating it (no repository, as in a Docker
  build context), the project loads with no reference and a warning.
- `git.changes(glob)` returns a list: the changed files since the merge
  base of `base` and `HEAD` that match `glob` and still exist, sorted,
  relative to the beam's `cwd`. Only in `run` and `env`; its argument is a
  literal or a `let`, and must equal one of the beam's rendered `inputs`.
- Full mode (the list is the glob's base directory, `src` for
  `src/**/*.php`, `.` without a literal prefix) whenever in doubt: no
  reference or `--no-affected`, a reference git cannot resolve (a warning),
  merge base at `HEAD` on a clean tree, the beam's Beamfile changed, a beam
  it `needs` is affected, or a changed path matches an input outside every
  `git.changes()` glob (`phpcs.xml` above). Otherwise incremental.
- `.dirs(glob)` maps each element to its shortest prefix matching `glob`
  (`*` never crosses `/`), keeps unmatched elements as they are, drops
  duplicates and elements under another kept one, and sorts. In full mode
  `src` stays `src`.
- A list renders as its elements joined by spaces, each shell-quoted for
  the executor: POSIX single quoting for the embedded shell, `sh`, docker,
  compose and plugins; PowerShell quoting and `\` separators for
  `system_shell` on Windows. Plain names (letters, digits, `_ . / : @ % + =
  , -`) stay bare; an element starting with `-` gets a `./` prefix. A
  list anywhere a string or boolean is required (`+`, `==`, `if`, `match`
  subject, `inputs`, `cwd`) is a load error.
- A beam whose every `git.changes()` call is empty in incremental mode
  does not run and ends `unchanged`. See `cli.md` for the flags.

## Embedded shell (the default executor)

Supported: `;`, `&&`, `||`, `!`, pipelines, redirections (`>`, `>>`, `<`,
`2>`, `2>>`, `2>&1`), POSIX quoting, `$VAR`, `${VAR}`, `$?`, `FOO=bar`
assignment, `FOO=bar cmd` prefix, `$(...)`, `~`, globs (`*`, `?`, `[...]`;
never a leading dot). A newline in a `run` string acts like `;`.

Builtins, which always win over a PATH binary of the same name: `cd`,
`pwd`, `exit`, `true`, `false`, `export`, `unset`, `echo` (`-n` only),
`cat`, `cp` (`-r`), `mv`, `rm` (`-r -f`), `mkdir` (`-p`), `touch`, `sleep`,
`test` / `[`. Use an explicit path (`/bin/rm`) to reach the system binary.

Since alba 0.6.0, the builtin `rm` refuses three operands even under `-f`:
the filesystem root (what `rm -rf $OUT_DIR/` names when `OUT_DIR` is
unset), one ending in `.` or `..`, and an empty one. Each refusal goes to
stderr and makes `rm` exit `1`; the other operands are still removed.

Rejected at `alba check` time: `if`, `for`, `while`, `case`, functions,
heredocs, `&`, `wait`, `(...)`, `${VAR:-default}`, `$((...))`, `{a,b}`,
and every special parameter but `$?`. Move the logic into a script, or
declare `executor system_shell` on that beam.

Frozen behaviours: an unset variable is empty (no `set -u`), a pipeline's
exit code is its last command's (no `pipefail`), an unmatched glob stays
literal.

## Executors

- **`system_shell`**: `sh` on Unix, PowerShell on Windows. No static check.
- **`docker { image "..." [build "..."] [dockerfile "..."] [volumes [...]]
  [workdir "..."] [user "..."] }`**: one container per beam, kept alive
  across the beam's commands, removed when the beam ends. The project
  directory is bind-mounted. `build`, `dockerfile`, `user`, and a
  relative volume path need alba 0.4.0 or later.
  - `volumes` entries are `host:container` with no `:ro` suffix; a
    container part starting with `./` is relative to the declaring
    Beamfile's directory (`"deps:./node_modules"`).
  - A host part with no slash is a named volume and an absolute one is
    mounted as declared; any other host part (`"../../libs:/libs"`) is
    relative to the declaring Beamfile's directory, so an imported
    module mounts the same directory from its own directory and from the
    root. Needs alba 0.5.1 or later: earlier versions resolve it against
    the directory alba runs from.
  - `build "docker/php"` builds the image from that context (relative to
    the declaring Beamfile's directory) before the beam runs, instead of
    naming one; `image` is then optional and tags the result. The cache
    reads the Dockerfile but not the rest of the context: list what the
    Dockerfile copies in `inputs`.
  - `dockerfile "Dockerfile.ci"` picks a file other than `Dockerfile`,
    relative to the build context; it requires `build`.
  - `user "host"` runs the commands as whoever runs Alba, so files
    written into the project are not owned by root; any other value goes
    to `docker run --user` as declared.
- **`compose { service "..." [file "..."] }`**: runs the beam in a one-off
  container of a service of the project's Compose file; everything else
  (image, build, mounts, user, environment) stays in the Compose file.
  `service` is required; `file` is the Compose file's path relative to
  the declaring Beamfile's directory, and without it Compose's own lookup
  applies (the beam's directory, then each parent). Needs alba 0.5.0 or
  later and `docker compose` on the `PATH`.
  - A command's host working directory maps to the container through
    the service's bind mounts (longest matching host path); if none
    covers it the command fails naming the `cwd` and the mounts.
  - The service's command is replaced, its ports are not published, no
    user is passed. The image needs `sh`.
  - Dependencies (`depends_on`) are started when absent, honouring their
    conditions, left alone when running, and put back in their previous
    state when the command ends. The cache follows the service's
    resolved configuration and the image or Dockerfile it names, not the
    rest of a build context.
  - A `kill -9` of Alba, or a second Ctrl-C, skips that restore: the
    dependencies Alba started stay up.
- **`<name> { option value ... }`**: an external `alba-executor-<name>`
  binary on the `PATH`. Resolution happens at plan time; a missing plugin
  fails the whole run before any beam starts.

Since alba 0.6.0, the cache fingerprints an executor's options and also
what they only name: the image a docker tag points at right now, or the
Dockerfile of a built image; a compose service's resolved configuration
plus the image or Dockerfile it names; the content of a plugin binary. An
image not pulled yet counts as its own state, so the run that pulls it is
followed by one more run before the cache settles. When that state cannot
be read (a stopped docker daemon), the beam runs uncached and says why.
Earlier versions only see the option text.

## Git hooks

```
hook pre-commit { beam fmt }
hook commit-msg { beam check_message }   # beam check_message(path) { ... }
```

`<name>` must be a git hook name. The target beam may declare at most as
many parameters as git passes to that hook. `alba hooks install` writes
scripts under `.alba/hooks/` and sets `core.hooksPath`; `alba hooks
uninstall` reverts it. `.alba/` belongs in `.gitignore`.
