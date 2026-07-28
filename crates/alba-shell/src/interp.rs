//! The interpreter: walks a parsed `Program` and produces a `ShellResult`.
//! Defines the crate's public runtime API (`ShellEnv`, `ShellOutputLine`,
//! `ShellStream`, `ShellResult`, `execute`), consumed directly by
//! `alba-executors` from Task 10 onward.

use std::path::PathBuf;

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::ast::{AndOrList, AndOrOp, Command, Pipeline, Program};
use crate::builtins;
use crate::expand::expand_word_literal;
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

/// A small sink builtins write whole output lines through, so they
/// don't need to know about channels or `ShellOutputLine` directly.
pub(crate) struct Lines<'a> {
    output: &'a UnboundedSender<ShellOutputLine>,
}

impl<'a> Lines<'a> {
    fn new(output: &'a UnboundedSender<ShellOutputLine>) -> Self {
        Self { output }
    }

    pub(crate) fn stdout(&self, text: impl Into<String>) {
        let _ = self.output.send(ShellOutputLine {
            stream: ShellStream::Stdout,
            text: text.into(),
        });
    }

    pub(crate) fn stderr(&self, text: impl Into<String>) {
        let _ = self.output.send(ShellOutputLine {
            stream: ShellStream::Stderr,
            text: text.into(),
        });
    }
}

/// Walks `program` to completion, returning its exit code. An empty
/// program exits 0.
pub async fn execute(program: &Program, env: ShellEnv) -> ShellResult {
    let mut state = ShellState::new(env.env, env.cwd);
    let flow = exec_program(program, &mut state, &env.output, &env.cancel).await;
    let exit_code = match flow {
        Flow::Next(code) | Flow::Exit(code) => code,
    };
    ShellResult { exit_code }
}

async fn exec_program(
    program: &Program,
    state: &mut ShellState,
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
        match exec_and_or_list(item, state, output, cancel).await {
            Flow::Exit(code) => return Flow::Exit(code),
            Flow::Next(next) => code = next,
        }
    }
    Flow::Next(code)
}

async fn exec_and_or_list(
    list: &AndOrList,
    state: &mut ShellState,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    let mut code = match exec_pipeline(&list.first, state, output, cancel).await {
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
        code = match exec_pipeline(pipeline, state, output, cancel).await {
            Flow::Exit(code) => return Flow::Exit(code),
            Flow::Next(code) => code,
        };
    }

    Flow::Next(code)
}

async fn exec_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    if cancel.is_cancelled() {
        return Flow::Exit(130);
    }

    if pipeline.commands.len() == 1 {
        return match exec_command(&pipeline.commands[0], state, output, cancel).await {
            Flow::Exit(code) => Flow::Exit(code),
            Flow::Next(code) => {
                let code = if pipeline.negated { negate(code) } else { code };
                state.last_exit = code;
                Flow::Next(code)
            }
        };
    }

    // Stub: a multi-stage pipeline (`a | b`) runs its stages
    // sequentially without connecting them and reports exit 0. Task 5
    // replaces this with real pipe wiring; do not test multi-stage
    // pipelines against this behaviour, it is intentionally temporary.
    for command in &pipeline.commands {
        if let Flow::Exit(code) = exec_command(command, state, output, cancel).await {
            return Flow::Exit(code);
        }
    }
    state.last_exit = 0;
    Flow::Next(0)
}

fn negate(code: i32) -> i32 {
    if code == 0 { 1 } else { 0 }
}

async fn exec_command(
    command: &Command,
    state: &mut ShellState,
    output: &UnboundedSender<ShellOutputLine>,
    cancel: &CancellationToken,
) -> Flow {
    let assignments: Vec<(String, String)> = command
        .assignments
        .iter()
        .map(|a| (a.name.clone(), expand_word_literal(&a.value, state)))
        .collect();
    let words: Vec<String> = command
        .words
        .iter()
        .map(|w| expand_word_literal(w, state))
        .collect();

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
    let io = Lines::new(output);

    let flow = if !has_path_separator(name)
        && let Some(builtin) = builtins::find(name)
    {
        builtins::run(builtin, args, state, &io)
    } else {
        run_external_command(name, args, state, output, cancel).await
    };

    overlay.restore(state);
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
    output: &UnboundedSender<ShellOutputLine>,
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
        let io = Lines::new(output);
        io.stderr(not_found_message(name));
        return Flow::Next(127);
    };

    let env = state.exported_env();
    let code = spawn::run_external(&path, args, &env, &state.cwd, output, cancel).await;

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

fn not_found_message(name: &str) -> String {
    match did_you_mean(name) {
        Some(candidate) => {
            format!("alba-shell: command not found: {name} (did you mean `{candidate}`?)")
        }
        None => format!("alba-shell: command not found: {name}"),
    }
}

/// The closest builtin name within edit distance 2, if any.
fn did_you_mean(name: &str) -> Option<&'static str> {
    builtins::NAMES
        .iter()
        .copied()
        .map(|candidate| (candidate, distance(name, candidate)))
        .filter(|(_, distance)| *distance <= 2)
        .min_by_key(|(_, distance)| *distance)
        .map(|(candidate, _)| candidate)
}

/// Damerau-Levenshtein edit distance (insert, delete, substitute, and
/// adjacent transposition all cost 1). Plain Levenshtein would leave a
/// transposed typo like `pdw` for `pwd` tied at distance 2 with an
/// unrelated word such as `cd`; counting the transposition as a single
/// edit — the classic did-you-mean touch — breaks that tie in favour of
/// the actual typo. No new dependency for a two-string comparison used
/// only for this diagnostic.
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
