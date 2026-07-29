//! The interpreter: walks a parsed `Program` and produces a `ShellResult`.
//! Defines the crate's public runtime API (`ShellEnv`, `ShellOutputLine`,
//! `ShellStream`, `ShellResult`, `execute`), consumed directly by
//! `alba-executors`.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::ast::{AndOrList, AndOrOp, Command, Pipeline, Program, Redirect};
use crate::builtins::{self, Builtin};
use crate::expand::{self, ExpandCtx};
use crate::io::{CommandIo, InTarget, OutTarget};
use crate::spawn;
use crate::state::{ShellState, Var};

/// Everything a run needs: the complete process environment, the
/// starting working directory, a sink for output lines, and a
/// cooperative cancellation signal.
///
/// `env` is the **complete** environment (the executor composes process
/// env + beam env before calling `execute`); every entry starts as an
/// exported shell variable.
pub struct ShellEnv {
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub output: UnboundedSender<ShellOutputLine>,
    pub cancel: CancellationToken,
}

/// One line of output from a builtin or an external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutputLine {
    pub stream: ShellStream,
    pub text: String,
}

/// Which stream an output line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStream {
    Stdout,
    Stderr,
}

/// The outcome of a run: the exit code of the program's last executed
/// command, or the code an `exit` builtin stopped it with.
pub struct ShellResult {
    pub exit_code: i32,
}

/// Internal control flow threaded through the walker: `Next` carries an
/// ordinary command's exit code and lets the walk continue; `Exit`
/// (from the `exit` builtin, or cancellation) stops the whole program.
pub(crate) enum Flow {
    Next(i32),
    Exit(i32),
}

/// Walks `program` to completion, returning its exit code. An empty
/// program exits 0.
pub async fn execute(program: &Program, env: ShellEnv) -> ShellResult {
    let mut state = ShellState::new(env.env, env.cwd);
    let stdout = OutTarget::Lines {
        tx: env.output.clone(),
        stream: ShellStream::Stdout,
    };
    let flow = exec_program(program, &mut state, &stdout, &env.output, &env.cancel).await;
    let exit_code = match flow {
        Flow::Next(code) | Flow::Exit(code) => code,
    };
    ShellResult { exit_code }
}

/// Runs `program` with its stdout captured into memory instead of sent
/// to the outer channel, for command substitution. Stderr still
/// forwards to `ctx.output` as it always does. Returns the exit code
/// (a frozen simplification: `expand.rs` discards it) and the captured
/// stdout, CRLF-normalized but with no trailing-newline stripping —
/// that trim is `expand.rs`'s job, since interior newlines must survive
/// for later splitting.
///
/// A plain `fn` returning a boxed future (not an `async fn`) so its body
/// can box the recursive descent back into `exec_program`: without that
/// indirection, the mutual recursion `exec_command` -> `expand::expand_words`
/// -> `execute_captured` -> `exec_program` -> ... -> `exec_command` would
/// require the compiler to lay out an infinitely-sized future type.
///
/// `Send` is spelled out rather than inferred for the same reason the
/// boxing is needed: `exec_stages` hands a stage's `exec_command` future
/// to `tokio::spawn`, which demands `Send`, and that obligation travels
/// right back around the same cycle. Naming it on the `dyn` here cuts
/// the loop the auto-trait inference would otherwise chase forever.
pub(crate) fn execute_captured<'a>(
    program: &'a Program,
    state: &'a mut ShellState,
    ctx: &'a ExpandCtx<'a>,
) -> Pin<Box<dyn Future<Output = (i32, String)> + Send + 'a>> {
    Box::pin(async move {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let stdout = OutTarget::Capture(buffer.clone());
        let flow = exec_program(program, state, &stdout, ctx.output, ctx.cancel).await;
        let code = match flow {
            Flow::Next(code) | Flow::Exit(code) => code,
        };
        let bytes = buffer.lock().expect("capture buffer poisoned").clone();
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        (code, text)
    })
}

/// `stdout` is the destination every command at this level writes its
/// stdout to unless a redirect or a pipe says otherwise: the run's
/// output channel, or a capture buffer inside a command substitution.
/// `output` is always the run's real output channel, which stderr and
/// any nested command substitution keep reaching regardless.
async fn exec_program(
    program: &Program,
    state: &mut ShellState,
    stdout: &OutTarget,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    let mut code = 0;
    for item in &program.items {
        // Checked between commands: a cancellation that lands in the
        // gap between two sequenced items must stop the program here,
        // without starting whatever comes next.
        if cancel.is_cancelled() {
            return Flow::Exit(130);
        }
        match exec_and_or_list(item, state, stdout, output, cancel).await {
            Flow::Exit(code) => return Flow::Exit(code),
            Flow::Next(next) => code = next,
        }
    }
    Flow::Next(code)
}

async fn exec_and_or_list(
    list: &AndOrList,
    state: &mut ShellState,
    stdout: &OutTarget,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    let mut code = match exec_pipeline(&list.first, state, stdout, output, cancel).await {
        Flow::Exit(code) => return Flow::Exit(code),
        Flow::Next(code) => code,
    };

    for (op, pipeline) in &list.rest {
        let should_run = match op {
            AndOrOp::And => code == 0,
            AndOrOp::Or => code != 0,
        };
        if !should_run {
            continue;
        }
        code = match exec_pipeline(pipeline, state, stdout, output, cancel).await {
            Flow::Exit(code) => return Flow::Exit(code),
            Flow::Next(code) => code,
        };
    }

    Flow::Next(code)
}

async fn exec_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
    stdout: &OutTarget,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    if cancel.is_cancelled() {
        return Flow::Exit(130);
    }

    let flow = if pipeline.commands.len() == 1 {
        // A lone command is the only shape that may change the shell:
        // it runs on the real state, and its `exit` really exits.
        match CommandIo::defaults(stdout, output) {
            Ok(io) => exec_command(&pipeline.commands[0], state, io, output, cancel).await,
            Err(error) => io_setup_failure(&error, output),
        }
    } else {
        exec_stages(pipeline, state, stdout, output, cancel).await
    };

    match flow {
        Flow::Exit(code) => Flow::Exit(code),
        Flow::Next(code) => {
            let code = if pipeline.negated { negate(code) } else { code };
            state.last_exit = code;
            Flow::Next(code)
        }
    }
}

/// Runs a multi-stage pipeline (`a | b | c`): every stage starts at
/// once, connected to its neighbours by an `os_pipe`, and the exit code
/// is the last stage's (no `pipefail`).
///
/// Each stage gets a **clone** of the shell state, so a `cd`, an
/// `export`, an `unset`, an assignment or an `exit` inside a stage
/// changes only that stage's own world — the shell it came from is
/// untouched, and an `exit` there merely becomes that stage's exit code.
///
/// Cancellation is the token, never the future: a cancelled run returns
/// here without waiting for a stage that has not noticed yet, and
/// dropping the `execute` future leaves every spawned stage running
/// detached. Callers must cancel the token, not drop the future.
async fn exec_stages(
    pipeline: &Pipeline,
    state: &ShellState,
    stdout: &OutTarget,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    let ios = match build_stage_io(pipeline.commands.len(), stdout, output) {
        Ok(ios) => ios,
        Err(error) => return io_setup_failure(&error, output),
    };

    // Spawned, not awaited one at a time: a stage that writes more than
    // its pipe can hold blocks until the next stage drains it, so every
    // stage has to be running before any of them is waited on.
    let mut stages = Vec::with_capacity(ios.len());
    for (command, io) in pipeline.commands.iter().zip(ios) {
        let command = command.clone();
        let mut state = state.clone();
        let output = output.clone();
        let cancel = cancel.clone();
        stages.push(tokio::spawn(async move {
            exec_command(&command, &mut state, io, &output, &cancel).await
        }));
    }

    let mut code = 0;
    for stage in stages {
        // Bounded by the token, not just by the stage: a builtin stage
        // runs on the blocking pool, which nothing can abort, so a `cat`
        // parked on a pipe a stray descendant still holds open would
        // otherwise keep this loop — and the whole run — waiting for as
        // long as that descendant lived, deaf to a Ctrl-C throughout.
        // The remaining stages are left detached, exactly as the external
        // path already leaves an abandoned drain reader.
        let flow = tokio::select! {
            joined = stage => joined.expect("pipeline stage panicked"),
            () = cancel.cancelled() => return Flow::Exit(130),
        };
        // `Flow::Exit` collapses to a plain code: an `exit` inside a
        // stage stops that stage, never the program.
        code = match flow {
            Flow::Next(stage_code) | Flow::Exit(stage_code) => stage_code,
        };
    }

    // Every stage honoured the same token while it ran (an external
    // through `spawn::run_external`'s terminate escalation, a builtin by
    // simply finishing); once they have all been joined, a cancelled run
    // stops here like any other.
    if cancel.is_cancelled() {
        return Flow::Exit(130);
    }
    Flow::Next(code)
}

/// Builds one [`CommandIo`] per stage, chaining them: stage *n*'s stdout
/// is the write end of a fresh pipe whose read end is stage *n+1*'s
/// stdin. The last stage keeps the pipeline's own stdout.
fn build_stage_io(
    stages: usize,
    stdout: &OutTarget,
    output: &UnboundedSender<ShellOutputLine>,
) -> std::io::Result<Vec<CommandIo>> {
    let mut ios = Vec::with_capacity(stages);
    let mut upstream: Option<os_pipe::PipeReader> = None;
    for stage in 0..stages {
        let mut io = CommandIo::defaults(stdout, output)?;
        if let Some(reader) = upstream.take() {
            io.stdin = InTarget::Pipe(reader);
        }
        if stage + 1 < stages {
            let (reader, writer) = os_pipe::pipe()?;
            upstream = Some(reader);
            io.stdout = OutTarget::Pipe(writer);
        }
        ios.push(io);
    }
    Ok(ios)
}

/// The shell could not even set up a command's streams (a pipe it could
/// not create, a descriptor it could not duplicate). Reported on the
/// run's own channel, since the io that would have carried it is exactly
/// what failed to exist.
fn io_setup_failure(error: &std::io::Error, output: &UnboundedSender<ShellOutputLine>) -> Flow {
    let _ = output.send(ShellOutputLine {
        stream: ShellStream::Stderr,
        text: format!("alba-shell: cannot set up command io: {error}"),
    });
    Flow::Next(1)
}

fn negate(code: i32) -> i32 {
    if code == 0 { 1 } else { 0 }
}

/// Runs one command on the streams `io` describes, after applying its
/// own redirections on top of them.
async fn exec_command(
    command: &Command,
    state: &mut ShellState,
    mut io: CommandIo,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    let ctx = ExpandCtx { output, cancel };

    let mut assignments = Vec::with_capacity(command.assignments.len());
    for assignment in &command.assignments {
        let value = match expand::expand_word_single(&assignment.value, state, &ctx).await {
            Ok(value) => value,
            Err(flow) => return flow,
        };
        assignments.push((assignment.name.clone(), value));
    }

    let words = match expand::expand_words(&command.words, state, &ctx).await {
        Ok(words) => words,
        Err(flow) => return flow,
    };

    let redirects = match resolve_redirects(&command.redirects, state, &ctx).await {
        Ok(redirects) => redirects,
        Err(flow) => return flow,
    };
    if let Err(message) = apply_redirects(&redirects, &state.cwd, &mut io) {
        // Reported through the io as redirected so far, so `cmd 2>err
        // >missing/out` puts the complaint where the command's stderr
        // was already pointed; the command itself never runs.
        let mut stderr = io.stderr.writer();
        let _ = writeln!(stderr, "{message}");
        return Flow::Next(1);
    }

    if words.is_empty() {
        // An assignment-only command (`FOO=bar`) sets unexported shell
        // vars permanently, unlike the temporary, exported overlay a
        // prefix on a real command gets below.
        for (name, value) in assignments {
            state.set(name, value);
        }
        return Flow::Next(0);
    }

    let overlay = AssignmentOverlay::apply(state, &assignments);
    let name = words[0].as_str();
    let args = &words[1..];

    let flow = if !has_path_separator(name)
        && let Some(builtin) = builtins::find(name)
    {
        run_builtin(builtin, args, state, io, cancel).await
    } else {
        run_external_command(name, args, state, io, cancel).await
    };

    overlay.restore(state);
    flow
}

/// A builtin's stream writes are synchronous. On the channel or a
/// capture buffer they never block, so the builtin runs inline; onto a
/// file or a pipe they can, and a blocking write on a runtime thread
/// would stall every other task — including, for a pipeline, the very
/// stage meant to drain that pipe. The state travels into the blocking
/// pool and back so a redirected `cd` or `export` still takes effect.
///
/// `sleep` never goes through any of that: it is dispatched straight to
/// [`builtins::run_sleep`], a genuinely asynchronous, cancellable wait,
/// so a cancellation lands the instant it fires rather than once a
/// blocking-pool thread happens to notice.
async fn run_builtin(
    builtin: Builtin,
    args: &[String],
    state: &mut ShellState,
    io: CommandIo,
    cancel: &CancellationToken,
) -> Flow {
    if builtin == Builtin::Sleep {
        return builtins::run_sleep(args, io, cancel).await;
    }

    if !io.can_block() {
        return builtins::run(builtin, args, state, io);
    }

    let args = args.to_vec();
    let mut owned = state.clone();
    let (flow, owned) = tokio::task::spawn_blocking(move || {
        let flow = builtins::run(builtin, &args, &mut owned, io);
        (flow, owned)
    })
    .await
    .expect("builtin panicked");
    *state = owned;
    flow
}

/// Resolves `name` to an external binary and runs it, or reports a
/// command-not-found / spawn failure. PATH lookup is skipped in favour
/// of a direct `cwd`-relative resolution when `name` contains a path
/// separator (`./script.sh`, `bin/tool`, an absolute path): an explicit
/// path always bypasses builtins, so it must also bypass PATH search.
async fn run_external_command(
    name: &str,
    args: &[String],
    state: &ShellState,
    io: CommandIo,
    cancel: &CancellationToken,
) -> Flow {
    let resolved = if has_path_separator(name) {
        // An explicit path (`./script.sh`, `bin/tool`) that does not
        // exist is "not found", not a spawn failure: 127, not 126. A
        // path that exists but is not executable (or otherwise fails to
        // spawn) still falls through to `spawn::run_external`'s own
        // 126 handling below.
        let candidate = state.cwd.join(name);
        candidate.exists().then_some(candidate)
    } else {
        which::which_in(name, state.get("PATH"), &state.cwd).ok()
    };

    let Some(path) = resolved else {
        let mut stderr = io.stderr.writer();
        let _ = writeln!(stderr, "{}", not_found_message(name, state.get("PATH")));
        return Flow::Next(127);
    };

    let env = state.exported_env();
    let code = spawn::run_external(&path, args, &env, &state.cwd, io, cancel).await;

    // A cancellation that arrived while the external was running is
    // reported the same way as one caught between commands: exit 130,
    // regardless of the raw code the killed process happened to exit
    // with (see `spawn::run_external`'s doc comment).
    if cancel.is_cancelled() {
        Flow::Exit(130)
    } else {
        Flow::Next(code)
    }
}

/// A redirection with its target already expanded. Splitting resolution
/// from application keeps the fallible, `async` half (expansion can run
/// a command substitution) apart from the synchronous half that opens
/// files, so the latter can stay a plain left-to-right loop.
enum ResolvedRedirect {
    Out {
        stderr: bool,
        append: bool,
        target: String,
    },
    In {
        target: String,
    },
    StderrToStdout,
}

async fn resolve_redirects(
    redirects: &[Redirect],
    state: &mut ShellState,
    ctx: &ExpandCtx<'_>,
) -> Result<Vec<ResolvedRedirect>, Flow> {
    let mut resolved = Vec::with_capacity(redirects.len());
    for redirect in redirects {
        resolved.push(match redirect {
            Redirect::Out {
                stderr,
                append,
                target,
            } => ResolvedRedirect::Out {
                stderr: *stderr,
                append: *append,
                target: expand::expand_word_single(target, state, ctx).await?,
            },
            Redirect::In { target } => ResolvedRedirect::In {
                target: expand::expand_word_single(target, state, ctx).await?,
            },
            Redirect::StderrToStdout => ResolvedRedirect::StderrToStdout,
        });
    }
    Ok(resolved)
}

/// Applies `redirects` to `io` left to right, resolving relative targets
/// against `cwd`. `Err` carries the user-facing message for the first
/// target that could not be opened; the caller reports it and does not
/// run the command.
fn apply_redirects(
    redirects: &[ResolvedRedirect],
    cwd: &Path,
    io: &mut CommandIo,
) -> Result<(), String> {
    for redirect in redirects {
        match redirect {
            ResolvedRedirect::Out {
                stderr,
                append,
                target,
            } => {
                let file = open_for_write(&cwd.join(target), *append)
                    .map_err(|error| cannot_open(target, &error))?;
                if *stderr {
                    io.stderr = OutTarget::File(file);
                } else {
                    io.stdout = OutTarget::File(file);
                }
            }
            ResolvedRedirect::In { target } => {
                let file = std::fs::File::open(cwd.join(target))
                    .map_err(|error| cannot_open(target, &error))?;
                io.stdin = InTarget::File(file);
            }
            // A clone of stdout *as it stands now*, so `> out 2>&1`
            // sends stderr to the file while `2>&1 > out` leaves it on
            // the original stdout — and, when that is the output
            // channel, tagged `Stdout` like everything else going there.
            ResolvedRedirect::StderrToStdout => {
                io.stderr = io
                    .stdout
                    .try_clone()
                    .map_err(|error| cannot_open("&1", &error))?;
            }
        }
    }
    Ok(())
}

fn open_for_write(path: &Path, append: bool) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)
}

fn cannot_open(target: &str, error: &std::io::Error) -> String {
    format!("alba-shell: cannot open {target}: {error}")
}

fn not_found_message(name: &str, path: Option<&str>) -> String {
    match did_you_mean(name, path) {
        Some(candidate) => {
            format!("alba-shell: command not found: {name} (did you mean `{candidate}`?)")
        }
        None => format!("alba-shell: command not found: {name}"),
    }
}

/// The closest command name to `name`, searched across the builtins and
/// every entry on `PATH` — a typo on an installed tool (`carg build`)
/// being the far more common mistake than a typo on a builtin.
///
/// Ties break on the name rather than on iteration order, which no
/// filesystem promises, so the same typo always draws the same
/// suggestion.
fn did_you_mean(name: &str, path: Option<&str>) -> Option<String> {
    let builtins = builtins::NAMES.iter().map(|name| (*name).to_string());
    builtins
        .chain(path_command_names(path))
        .map(|candidate| {
            let distance = distance(name, &candidate);
            (candidate, distance)
        })
        .filter(|(candidate, distance)| is_plausible_typo(name, candidate, *distance))
        .min_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
        .map(|(candidate, _)| candidate)
}

/// Whether `candidate` is close enough to `name` to be worth offering.
/// One edit always is. Two edits only count when both names are at least
/// six characters, long enough for two edits to still read as a typo
/// rather than as a different word: at distance 2 alone, `nosuch` drew
/// `touch` and, on windows, every ordinary unix command name drew some
/// unrelated builtin. Distance 0 is not a typo at all — a `PATH` entry
/// spelled exactly like the name that just failed to resolve is a file
/// that is not executable, and suggesting it back would say nothing.
fn is_plausible_typo(name: &str, candidate: &str, distance: usize) -> bool {
    match distance {
        1 => true,
        2 => name.chars().count() >= 6 && candidate.chars().count() >= 6,
        _ => false,
    }
}

/// Every file name found on `PATH`. A spelling hint, not a resolution:
/// `which` has already searched and failed, so an entry is offered
/// without re-checking that it is really executable. On windows the
/// executable extension is dropped, so a typo on `cargo` suggests
/// `cargo` rather than `cargo.exe`.
fn path_command_names(path: Option<&str>) -> impl Iterator<Item = String> {
    let dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    dirs.into_iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| command_name(&entry.ok()?.file_name()))
}

fn command_name(file_name: &std::ffi::OsStr) -> Option<String> {
    let name = file_name.to_str()?;
    if cfg!(windows) {
        return Some(Path::new(name).file_stem()?.to_string_lossy().into_owned());
    }
    Some(name.to_string())
}

/// Optimal string alignment distance (insert, delete, substitute, and
/// adjacent transposition all cost 1, with no substring edited twice).
/// Plain Levenshtein would leave a transposed typo like `pdw` for `pwd`
/// tied at distance 2 with an unrelated word such as `cd`; counting the
/// transposition as a single edit — the classic did-you-mean touch —
/// breaks that tie in favour of the actual typo. No new dependency for a
/// two-string comparison used only for this diagnostic.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in dp.iter_mut().enumerate() {
        row[0] = i;
    }
    if let Some(first_row) = dp.first_mut() {
        for (j, cell) in first_row.iter_mut().enumerate() {
            *cell = j;
        }
    }

    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                dp[i][j] = dp[i][j].min(dp[i - 2][j - 2] + 1);
            }
        }
    }

    dp[a.len()][b.len()]
}

fn has_path_separator(name: &str) -> bool {
    name.contains('/') || name.contains(std::path::MAIN_SEPARATOR)
}

/// Applies a command's `NAME=value` assignment prefixes as a temporary,
/// exported overlay on the shell state for the duration of that one
/// command, then restores whatever was there before.
struct AssignmentOverlay {
    saved: Vec<(String, Option<Var>)>,
}

impl AssignmentOverlay {
    fn apply(state: &mut ShellState, assignments: &[(String, String)]) -> Self {
        let mut saved = Vec::with_capacity(assignments.len());
        for (name, value) in assignments {
            saved.push((name.clone(), state.vars.get(name).cloned()));
            state.export(name, Some(value.clone()));
        }
        Self { saved }
    }

    fn restore(self, state: &mut ShellState) {
        // Undo in reverse: a repeated name in the same prefix (`FOO=1
        // FOO=2 cmd`) pushes one saved entry per assignment, each
        // capturing what was there *before that particular assignment*
        // ran. Replaying them in the order they were saved would apply
        // the oldest snapshot last, leaving the intermediate value
        // (`FOO=1`) behind, still exported, instead of the true original.
        // Replaying newest-saved-first peels each overlay off in the
        // opposite order it was applied, so the last entry undone is the
        // very first assignment's saved prior value — the actual
        // original.
        for (name, prior) in self.saved.into_iter().rev() {
            match prior {
                Some(var) => {
                    state.vars.insert(name, var);
                }
                None => {
                    state.vars.remove(&name);
                }
            }
        }
    }
}
