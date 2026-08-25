//! Expression evaluation, template rendering, and the [`Scope`] they run
//! against — plus the single-file `load_str` orchestration that walks a
//! parsed [`File`] into a [`Project`].
//!
//! ## Load-time vs. schedule-time rendering
//!
//! A beam's `description`, `inputs`, `outputs`, `cwd`, and executor
//! options are rendered once, here, at load time, using a [`Scope`] built
//! only from the file's `let` bindings. A beam's own parameter is never in
//! that scope, so referencing one there is always an error — but a
//! same-named `let` could otherwise silently take its place instead of
//! erroring, which is why `render_load_time_template` explicitly rejects
//! any `Var` matching one of the beam's parameter names (via
//! `reject_param_references`) *before* rendering, rather than relying on
//! the scope's shape alone.
//!
//! `run` and `env` values stay as [`StringTemplate`]s in the model,
//! rendered later (at schedule time, by the engine, through the same
//! [`render_template`] used here) against a [`Scope`] that also has the
//! beam's parameters bound via [`Scope::with_params`]. Because parameters
//! aren't bound yet at load time, those templates can't be fully
//! evaluated up front — but every name they reference must still resolve
//! to *something* (a file-level `let` or one of the beam's own
//! parameters) at load time.
//!
//! ## Validation is a pass of its own, on both paths
//!
//! [`validate_template`] walks an expression tree via [`check_expr`]
//! without evaluating it, treating a reference to a file-level `let` as
//! concretely known (since those are already evaluated; a same-named
//! parameter still shadows it, matching [`Scope::with_params`]) and a
//! reference to one of the beam's own parameters as
//! validly-named-but-unknown-until-schedule-time, and eagerly type-checks
//! (reusing [`eval_binary`]) any sub-expression that turns out to depend
//! only on `let`s. That is what makes `if_condition_must_be_bool` (whose
//! `if` condition is a plain `let`, with no parameter involved) catchable
//! at load time, while a `run` template that mixes `let`s and parameters
//! only gets its names checked, not its types, until schedule time.
//!
//! Crucially this pass runs for load-time fields too, *in addition to*
//! evaluating them. Evaluation alone walks only the branch an `if`
//! condition selects, and which branch that is can depend on the
//! environment the file happens to be loaded in (a condition derived from
//! `env(...)`, say). Validating only the taken branch would therefore make
//! a diagnostic environment-dependent: a typo hiding in the other branch
//! would fail on one machine and pass on the next, for a file nobody
//! edited. [`check_expr`] validates every branch regardless.
//!
//! ## `env()` and where it may appear
//!
//! `env()` reads the process environment, and [`check_expr`] never
//! executes it: on the schedule-time path the read belongs to the moment
//! the beam runs, not to loading. On the load-time path the read does
//! happen (that is what `let profile = env("PROFILE", "debug")` is for),
//! but only with a default in hand: `env(name)` with no default is
//! rejected at its call site there, because otherwise one unset variable
//! would make the whole project unloadable — including for beams nobody
//! asked to run — and `alba check` would answer differently depending on
//! the machine. Without a default, `env()` belongs in `run` or `env`,
//! where it resolves when the beam runs. `glob()` has no such side effect
//! (it only compiles a pattern), so it is validated eagerly on both paths
//! as soon as its argument is known.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use alba_syntax::{
    BeamDecl, BeamRef, BinOp, ExecutorDecl, ExecutorOptionValue, Expr, File, ParseError, Span,
    Spanned, StringTemplate, TemplatePart,
};

use crate::error::{CoreError, ROOT_SOURCE_ID, SourceIdScope, current_source_id};
use crate::git::{GitError, GitHead};
use crate::model::{Beam, BeamId, ExecutorKind, OptionValue, Project, Value};

/// The built-in functions `eval_expr` recognizes in a `Call` expression.
const BUILTIN_FUNCTIONS: &[&str] = &["env", "glob"];

/// Name bindings available while evaluating expressions: a file's
/// evaluated `let`s, later composed with a beam's own parameter values.
///
/// Cloning is a plain `HashMap` clone. A Beamfile binds a handful of
/// `let`s and a beam takes a handful of parameters, so this stays cheap in
/// practice; a persistent/structural-sharing map would be premature
/// machinery for that size.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scope {
    values: HashMap<String, Value>,
    git: Option<Arc<LazyGit>>,
}

/// The `git` object's values, computed on first access and shared by every
/// scope of one load: a Beamfile that never reads `git` never spawns it,
/// and one that reads it in ten places spawns it once.
#[derive(Debug)]
pub(crate) struct LazyGit {
    root: PathBuf,
    head: OnceLock<Result<GitHead, GitError>>,
}

impl LazyGit {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            head: OnceLock::new(),
        }
    }

    fn head(&self) -> Result<&GitHead, GitError> {
        self.head
            .get_or_init(|| crate::git::head(&self.root))
            .as_ref()
            .map_err(Clone::clone)
    }
}

/// Two scopes from the same load share one `LazyGit`; comparing the root
/// is what "same git" means, and keeps `Scope: PartialEq`.
impl PartialEq for LazyGit {
    fn eq(&self, other: &Self) -> bool {
        self.root == other.root
    }
}

impl Scope {
    /// An empty scope: no `let`s, no parameters.
    pub fn empty() -> Self {
        Self::default()
    }

    /// A scope with no `let`s or parameters yet, but with `git` wired up
    /// so `git.*` field access resolves once an expression reads it.
    pub(crate) fn with_git(git: Arc<LazyGit>) -> Self {
        Self {
            values: HashMap::new(),
            git: Some(git),
        }
    }

    /// The `git.<field>` value, or the error to report at `span`.
    fn git_field(&self, field: &Spanned<String>, span: Span) -> Result<Value, CoreError> {
        let Some(git) = &self.git else {
            return Err(CoreError::new("`git` is not available here", span));
        };
        // The field name is checked before `git.head()` runs, so an
        // unknown field like `git.tag` fails without spawning git.
        if !matches!(
            field.value.as_str(),
            "branch" | "sha" | "short_sha" | "dirty"
        ) {
            return Err(
                CoreError::new(format!("unknown git field `{}`", field.value), field.span)
                    .with_help("expected one of `branch`, `sha`, `short_sha`, `dirty`"),
            );
        }
        let head = git.head().map_err(|error| {
            CoreError::new(format!("cannot read `git.{}`: {error}", field.value), span)
        })?;
        Ok(match field.value.as_str() {
            "branch" => Value::Str(head.branch.clone()),
            "sha" => Value::Str(head.sha.clone()),
            "short_sha" => Value::Str(head.short_sha.clone()),
            "dirty" => Value::Bool(head.dirty),
            _ => unreachable!("checked above"),
        })
    }

    /// Binds `name` to `value` in place. `pub(crate)`: only this module's
    /// `let`-chain evaluation builds a scope up incrementally; external
    /// callers only ever compose scopes via [`Scope::with_params`].
    pub(crate) fn insert(&mut self, name: String, value: Value) {
        self.values.insert(name, value);
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.values.get(name)
    }

    /// The names currently bound in this scope, for "did you mean...?"
    /// suggestions on an unknown-variable error.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// Returns a new scope with `params[i]` bound to `args[i]` for each
    /// `i`, layered on top of `self`'s existing bindings (a parameter name
    /// that collides with a `let` shadows it for the new scope). This is
    /// the cheap, obvious composition the engine uses to bind a beam's
    /// positional arguments before rendering its `run`/`env` templates:
    /// `beam.scope.with_params(&beam.params, &args)`.
    ///
    /// Mismatched lengths are not validated here: `zip` simply stops at
    /// the shorter of the two, so a missing argument leaves its parameter
    /// name unbound (rendering later fails with a plain "unknown
    /// variable" error, not an arity message) and an extra argument is
    /// silently ignored. The engine checks `args.len() ==
    /// beam.params.len()` itself before calling this, so a real
    /// argument-count mismatch gets a clear diagnostic instead of
    /// surfacing here as a confusing unknown-variable one.
    pub fn with_params(&self, params: &[String], args: &[String]) -> Self {
        let mut values = self.values.clone();
        for (name, arg) in params.iter().zip(args.iter()) {
            values.insert(name.clone(), Value::Str(arg.clone()));
        }
        Self {
            values,
            git: self.git.clone(),
        }
    }
}

/// The most precise span available for `expr`, per `alba_syntax`'s span
/// policy: `Str` (via its `StringTemplate`), `Var`, and `Call` (via their
/// name) carry one; `Bool`, `Binary`, and `If` do not. Callers fall back to
/// an enclosing span (a `StringTemplate`'s span, or a fallback threaded
/// down from one) when this returns `None`.
fn expr_span(expr: &Expr) -> Option<Span> {
    match expr {
        Expr::Str(t) => Some(t.span),
        Expr::Var(name) => Some(name.span),
        Expr::Call { name, .. } => Some(name.span),
        Expr::Bool(_) | Expr::Binary { .. } | Expr::If { .. } => None,
        Expr::Field { object, field } => Some(Span::new(object.span.start, field.span.end)),
    }
}

/// `object.field`: only `git` has fields. Shared by evaluation and
/// load-time checking, which agree on the answer since the values are
/// known as soon as git answers.
fn eval_field(
    object: &Spanned<String>,
    field: &Spanned<String>,
    scope: &Scope,
) -> Result<Value, CoreError> {
    if object.value != "git" {
        return Err(CoreError::new(
            format!("`{}` has no fields; only `git` does", object.value),
            object.span,
        ));
    }
    scope.git_field(field, Span::new(object.span.start, field.span.end))
}

/// Classic dynamic-programming Levenshtein edit distance between two
/// strings, operating on `char`s so it stays correct for non-ASCII names.
/// Duplicated from `alba_syntax::parser`'s private `levenshtein` (not
/// exported across the crate boundary) rather than shared, so this crate's
/// "did you mean...?" suggestions use the exact same algorithm and
/// distance-2 threshold as the parser's unknown-field suggestions, for a
/// consistent feel across the DSL's diagnostics.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev_diag = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let temp = row[j];
            row[j] = if a[i - 1] == b[j - 1] {
                prev_diag
            } else {
                1 + prev_diag.min(row[j]).min(row[j - 1])
            };
            prev_diag = temp;
        }
    }
    row[b.len()]
}

/// The closest name in `candidates` to `name`, if within Levenshtein
/// distance 2, matching `alba_syntax`'s unknown-field suggestion
/// threshold. Ties are broken alphabetically, not by iteration order:
/// `candidates` is typically a `HashMap`'s keys (via `Scope::names`),
/// whose iteration order is randomized per process, so without an
/// explicit tie-break the suggestion offered for the same typo could
/// change from run to run.
///
/// `pub`: `graph.rs`'s "unknown beam" and "unknown target" diagnostics
/// reuse this exact function (and its tie-break rule) for their own "did
/// you mean...?" help, rather than duplicating it a third time alongside
/// `alba_syntax::parser`'s private original — and `alba-engine`'s plugin
/// resolution reuses it again for a missing `alba-executor-<name>`
/// binary's own did-you-mean help.
pub fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut scored: Vec<(&str, usize)> = candidates
        .map(|candidate| (candidate, levenshtein(name, candidate)))
        .filter(|&(_, distance)| distance <= 2)
        .collect();
    scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
    scored.into_iter().next().map(|(candidate, _)| candidate)
}

fn unknown_variable_error<'a>(
    name: &Spanned<String>,
    candidates: impl Iterator<Item = &'a str>,
) -> CoreError {
    let err = CoreError::new(format!("unknown variable `{}`", name.value), name.span);
    match suggest(&name.value, candidates) {
        Some(c) => err.with_help(format!("did you mean `{c}`?")),
        None => err,
    }
}

fn unknown_function_error(name: &Spanned<String>) -> CoreError {
    let err = CoreError::new(format!("unknown function `{}`", name.value), name.span);
    match suggest(&name.value, BUILTIN_FUNCTIONS.iter().copied()) {
        Some(c) => err.with_help(format!("did you mean `{c}`?")),
        None => err,
    }
}

/// Renders `template` against `scope`: literal parts pass through
/// unchanged, `{expr}` parts are evaluated and interpolated (strings
/// verbatim, booleans as `true`/`false`).
pub fn render_template(template: &StringTemplate, scope: &Scope) -> Result<String, CoreError> {
    let mut out = String::new();
    for part in &template.parts {
        match part {
            TemplatePart::Literal(s) => out.push_str(s),
            TemplatePart::Expr(e) => {
                let fallback = expr_span(e).unwrap_or(template.span);
                let value = eval_with_fallback(e, scope, fallback)?;
                out.push_str(value.as_display());
            }
        }
    }
    Ok(out)
}

/// Evaluates `expr` against `scope`. The public entry point; internally
/// delegates to a private helper that threads a span to fall back on for
/// spanless expression kinds (`Bool`, `Binary`, `If`) down through
/// recursive calls. Called directly (rather than through
/// [`render_template`] or the deferred-template checker below, both of
/// which have richer context to use as that fallback), the best available
/// fallback is `expr`'s own span if it has one, or a zero-width span at
/// the start of the source as a last resort.
pub fn eval_expr(expr: &Expr, scope: &Scope) -> Result<Value, CoreError> {
    let fallback = expr_span(expr).unwrap_or(Span::new(0, 0));
    eval_with_fallback(expr, scope, fallback)
}

fn eval_with_fallback(expr: &Expr, scope: &Scope, fallback: Span) -> Result<Value, CoreError> {
    match expr {
        Expr::Str(t) => Ok(Value::Str(render_template(t, scope)?)),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Var(name) => scope
            .get(&name.value)
            .cloned()
            .ok_or_else(|| unknown_variable_error(name, scope.names())),
        Expr::Call { name, args } => eval_call(name, args, scope, fallback),
        Expr::Binary { op, lhs, rhs } => {
            let lv = eval_with_fallback(lhs, scope, expr_span(lhs).unwrap_or(fallback))?;
            let rv = eval_with_fallback(rhs, scope, expr_span(rhs).unwrap_or(fallback))?;
            eval_binary(*op, lv, rv, fallback)
        }
        Expr::If {
            cond,
            then,
            otherwise,
        } => {
            let cond_span = expr_span(cond).unwrap_or(fallback);
            match eval_with_fallback(cond, scope, cond_span)? {
                Value::Bool(true) => {
                    eval_with_fallback(then, scope, expr_span(then).unwrap_or(fallback))
                }
                Value::Bool(false) => {
                    eval_with_fallback(otherwise, scope, expr_span(otherwise).unwrap_or(fallback))
                }
                other => Err(CoreError::new(
                    format!(
                        "expected a boolean condition for `if`, found {}",
                        other.type_name()
                    ),
                    cond_span,
                )),
            }
        }
        Expr::Field { object, field } => eval_field(object, field, scope),
    }
}

fn eval_binary(op: BinOp, lhs: Value, rhs: Value, span: Span) -> Result<Value, CoreError> {
    match op {
        BinOp::Concat => match (lhs, rhs) {
            (Value::Str(a), Value::Str(b)) => Ok(Value::Str(a + &b)),
            (l, r) => Err(CoreError::new(
                format!(
                    "`+` requires two strings, found {} and {}",
                    l.type_name(),
                    r.type_name()
                ),
                span,
            )),
        },
        BinOp::Eq | BinOp::NotEq => {
            if std::mem::discriminant(&lhs) != std::mem::discriminant(&rhs) {
                return Err(CoreError::new(
                    format!(
                        "`==`/`!=` requires operands of the same type, found {} and {}",
                        lhs.type_name(),
                        rhs.type_name()
                    ),
                    span,
                ));
            }
            let equal = lhs == rhs;
            Ok(Value::Bool(if op == BinOp::Eq { equal } else { !equal }))
        }
        BinOp::And | BinOp::Or => match (lhs, rhs) {
            (Value::Bool(a), Value::Bool(b)) => {
                Ok(Value::Bool(if op == BinOp::And { a && b } else { a || b }))
            }
            (l, r) => Err(CoreError::new(
                format!(
                    "`&&`/`||` requires two booleans, found {} and {}",
                    l.type_name(),
                    r.type_name()
                ),
                span,
            )),
        },
    }
}

fn eval_call(
    name: &Spanned<String>,
    args: &[Expr],
    scope: &Scope,
    fallback: Span,
) -> Result<Value, CoreError> {
    match name.value.as_str() {
        "env" => eval_env(name, args, scope, fallback),
        "glob" => eval_glob(name, args, scope, fallback),
        _ => Err(unknown_function_error(name)),
    }
}

fn arity_error(name: &Spanned<String>, min: usize, max: usize, found: usize) -> CoreError {
    let expected = if min == max {
        format!("{min}")
    } else {
        format!("{min} to {max}")
    };
    CoreError::new(
        format!(
            "`{}` expects {expected} argument(s), found {found}",
            name.value
        ),
        name.span,
    )
}

/// Requires `value` to be a string, for built-in function arguments
/// (`env`'s name/default, `glob`'s pattern) that are never meaningfully
/// anything else. Shared by [`expect_str_arg`] (which evaluates the
/// argument first) and [`check_deferred_expr`]'s `glob` handling (which
/// already has the argument's statically known value in hand, and would
/// otherwise have to re-evaluate it to reach this same check).
fn expect_str_value(fn_name: &str, value: Value, span: Span) -> Result<String, CoreError> {
    match value {
        Value::Str(s) => Ok(s),
        other => Err(CoreError::new(
            format!(
                "argument to `{fn_name}` must be a string, found {}",
                other.type_name()
            ),
            span,
        )),
    }
}

/// Evaluates `arg` and requires the result to be a string.
fn expect_str_arg(
    fn_name: &Spanned<String>,
    arg: &Expr,
    scope: &Scope,
    fallback: Span,
) -> Result<String, CoreError> {
    let span = expr_span(arg).unwrap_or(fallback);
    let value = eval_with_fallback(arg, scope, span)?;
    expect_str_value(&fn_name.value, value, span)
}

/// `env(name)` / `env(name, default)`: reads the process environment.
fn eval_env(
    name: &Spanned<String>,
    args: &[Expr],
    scope: &Scope,
    fallback: Span,
) -> Result<Value, CoreError> {
    if args.is_empty() || args.len() > 2 {
        return Err(arity_error(name, 1, 2, args.len()));
    }
    let key = expect_str_arg(name, &args[0], scope, fallback)?;
    match std::env::var(&key) {
        Ok(v) => Ok(Value::Str(v)),
        Err(_) => match args.get(1) {
            Some(default_expr) => {
                let span = expr_span(default_expr).unwrap_or(fallback);
                eval_with_fallback(default_expr, scope, span)
            }
            None => Err(CoreError::new(
                format!("environment variable `{key}` is not set"),
                name.span,
            )),
        },
    }
}

/// Validates that `pattern` compiles as a glob pattern (expansion is the
/// cache's job, later) and returns it unchanged, wrapped as a [`Value`].
/// Shared by [`eval_glob`] and [`check_deferred_expr`]'s `glob` handling.
fn validate_glob_pattern(pattern: String, span: Span) -> Result<Value, CoreError> {
    glob::Pattern::new(&pattern)
        .map_err(|e| CoreError::new(format!("invalid glob pattern `{pattern}`: {e}"), span))?;
    Ok(Value::Str(pattern))
}

/// `glob(pattern)`: validates that the pattern compiles and returns it
/// unchanged as a string.
fn eval_glob(
    name: &Spanned<String>,
    args: &[Expr],
    scope: &Scope,
    fallback: Span,
) -> Result<Value, CoreError> {
    if args.len() != 1 {
        return Err(arity_error(name, 1, 1, args.len()));
    }
    let pattern_span = expr_span(&args[0]).unwrap_or(fallback);
    let pattern = expect_str_arg(name, &args[0], scope, fallback)?;
    validate_glob_pattern(pattern, pattern_span)
}

/// What [`check_expr`] statically knows about a sub-expression: either its
/// concrete value (it only touched file-level `let`s, which are already
/// evaluated) or that it depends on something not known yet — one of the
/// beam's own parameters, or the process environment.
enum StaticValue {
    Known(Value),
    Unknown,
}

/// When the expression being validated will actually be evaluated, which
/// is the one thing [`check_expr`] treats differently between the two
/// paths: whether `env()` may be written without a default. See the module
/// doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Timing {
    /// `let` bindings and the beam fields evaluated during the load itself
    /// (`description`, `inputs`, `outputs`, `cwd`, executor options).
    Load,
    /// `run` commands and `env` values, rendered when the beam is about to
    /// run, with its parameters bound.
    Schedule,
}

/// Walks `expr`, validating variable and function names and eagerly
/// type-checking whatever sub-expressions don't depend on `params` or on
/// the environment. See the module doc comment for the overall strategy —
/// in particular, *every* branch of an `if` is validated here, even one a
/// statically known condition doesn't take, and `env()` is never actually
/// executed on this path.
fn check_expr(
    expr: &Expr,
    lets: &Scope,
    params: &[String],
    timing: Timing,
    fallback: Span,
) -> Result<StaticValue, CoreError> {
    match expr {
        Expr::Bool(b) => Ok(StaticValue::Known(Value::Bool(*b))),
        Expr::Var(name) => {
            // Parameters are checked first: `Scope::with_params` has a
            // same-named parameter shadow a `let` at schedule time, so a
            // shadowed `let`'s value must not be used here either — doing
            // so would validate against a value the beam will never
            // actually see once its parameter is bound.
            if params.iter().any(|p| p == &name.value) {
                Ok(StaticValue::Unknown)
            } else if let Some(v) = lets.get(&name.value) {
                Ok(StaticValue::Known(v.clone()))
            } else {
                let candidates = lets.names().chain(params.iter().map(String::as_str));
                Err(unknown_variable_error(name, candidates))
            }
        }
        Expr::Str(template) => {
            // Builds the rendered string directly from each part's
            // already-computed `StaticValue` rather than recursing once
            // to check it and then calling `render_template` again on the
            // same subtree, which would (among other redundant work)
            // evaluate any `glob()` call inside it twice.
            let mut any_unknown = false;
            let mut rendered = String::new();
            for part in &template.parts {
                match part {
                    TemplatePart::Literal(s) => rendered.push_str(s),
                    TemplatePart::Expr(e) => {
                        let span = expr_span(e).unwrap_or(template.span);
                        match check_expr(e, lets, params, timing, span)? {
                            StaticValue::Known(v) => rendered.push_str(v.as_display()),
                            StaticValue::Unknown => any_unknown = true,
                        }
                    }
                }
            }
            if any_unknown {
                Ok(StaticValue::Unknown)
            } else {
                Ok(StaticValue::Known(Value::Str(rendered)))
            }
        }
        // Dispatched by name, exactly like `eval_call`, so adding a third
        // built-in cannot silently fall through into another one's
        // argument handling.
        Expr::Call { name, args } => match name.value.as_str() {
            "env" => check_env_call(name, args, lets, params, timing, fallback),
            "glob" => check_glob_call(name, args, lets, params, timing, fallback),
            _ => Err(unknown_function_error(name)),
        },
        Expr::Binary { op, lhs, rhs } => {
            let l = check_expr(
                lhs,
                lets,
                params,
                timing,
                expr_span(lhs).unwrap_or(fallback),
            )?;
            let r = check_expr(
                rhs,
                lets,
                params,
                timing,
                expr_span(rhs).unwrap_or(fallback),
            )?;
            match (l, r) {
                (StaticValue::Known(lv), StaticValue::Known(rv)) => {
                    Ok(StaticValue::Known(eval_binary(*op, lv, rv, fallback)?))
                }
                // At least one operand depends on a parameter: its real
                // type is only known once that parameter is bound, so skip
                // type-checking this operation until schedule time.
                _ => Ok(StaticValue::Unknown),
            }
        }
        Expr::If {
            cond,
            then,
            otherwise,
        } => {
            let cond_span = expr_span(cond).unwrap_or(fallback);
            let then_span = expr_span(then).unwrap_or(fallback);
            let otherwise_span = expr_span(otherwise).unwrap_or(fallback);
            match check_expr(cond, lets, params, timing, cond_span)? {
                StaticValue::Known(Value::Bool(cond_value)) => {
                    // Both branches are validated regardless of which one
                    // the condition picks: an untaken branch can still
                    // reference a nonexistent name, and which branch is
                    // "untaken" can depend on the environment the file
                    // happens to be loaded in (e.g. a condition derived
                    // from `env(...)`) — that must not make the
                    // diagnostic environment-dependent too.
                    let then_result = check_expr(then, lets, params, timing, then_span)?;
                    let else_result = check_expr(otherwise, lets, params, timing, otherwise_span)?;
                    Ok(if cond_value { then_result } else { else_result })
                }
                StaticValue::Known(other) => Err(CoreError::new(
                    format!(
                        "expected a boolean condition for `if`, found {}",
                        other.type_name()
                    ),
                    cond_span,
                )),
                StaticValue::Unknown => {
                    // Which branch runs isn't decided yet; still validate
                    // names and types in both.
                    check_expr(then, lets, params, timing, then_span)?;
                    check_expr(otherwise, lets, params, timing, otherwise_span)?;
                    Ok(StaticValue::Unknown)
                }
            }
        }
        Expr::Field { object, field } => eval_field(object, field, lets).map(StaticValue::Known),
    }
}

/// Validates an `env(name)` / `env(name, default)` call without executing
/// it. Arity is checked before the arguments are walked, so a miscall is
/// reported as one however little is known about what it was given.
fn check_env_call(
    name: &Spanned<String>,
    args: &[Expr],
    lets: &Scope,
    params: &[String],
    timing: Timing,
    fallback: Span,
) -> Result<StaticValue, CoreError> {
    if args.is_empty() || args.len() > 2 {
        return Err(arity_error(name, 1, 2, args.len()));
    }
    for arg in args {
        let span = expr_span(arg).unwrap_or(fallback);
        check_expr(arg, lets, params, timing, span)?;
    }
    if timing == Timing::Load && args.len() == 1 {
        return Err(CoreError::new(
            "`env` requires a default value here",
            name.span,
        )
        .with_help(
            "add one, e.g. `env(\"NAME\", \"fallback\")`, or read the variable from `run`/`env` \
             instead, where it resolves when the beam runs",
        ));
    }
    // Never read here: at schedule time the read belongs to the moment the
    // beam runs, and at load time the value still depends on the ambient
    // environment, so nothing downstream may be type-checked against it.
    Ok(StaticValue::Unknown)
}

/// Validates a `glob(pattern)` call, compiling the pattern when it is
/// already known. Compiling has no side effect, so unlike `env` this one
/// really is evaluated here.
fn check_glob_call(
    name: &Spanned<String>,
    args: &[Expr],
    lets: &Scope,
    params: &[String],
    timing: Timing,
    fallback: Span,
) -> Result<StaticValue, CoreError> {
    if args.len() != 1 {
        return Err(arity_error(name, 1, 1, args.len()));
    }
    let pattern_span = expr_span(&args[0]).unwrap_or(fallback);
    match check_expr(&args[0], lets, params, timing, pattern_span)? {
        StaticValue::Known(value) => {
            let pattern = expect_str_value(&name.value, value, pattern_span)?;
            Ok(StaticValue::Known(validate_glob_pattern(
                pattern,
                pattern_span,
            )?))
        }
        StaticValue::Unknown => Ok(StaticValue::Unknown),
    }
}

/// Validates a template without rendering it: every `Var` it references
/// must be either a file-level `let` or one of `params` (the beam's own
/// parameters, unbound until schedule time), every branch of every `if`
/// included. See the module doc comment for why this also happens to catch
/// some type errors (like `if_condition_must_be_bool`) early.
fn validate_template(
    template: &StringTemplate,
    lets: &Scope,
    params: &[String],
    timing: Timing,
) -> Result<(), CoreError> {
    for part in &template.parts {
        if let TemplatePart::Expr(e) = part {
            let span = expr_span(e).unwrap_or(template.span);
            check_expr(e, lets, params, timing, span)?;
        }
    }
    Ok(())
}

/// Rejects any reference to one of `params` inside `expr`, tagged with
/// `field` for the error message. Used for load-time fields
/// (`description`, `inputs`, `outputs`, `cwd`, executor options), where a
/// beam's own parameters are never in scope — but without this explicit
/// check, a same-named `let` would silently take a forbidden parameter
/// reference's place instead of erroring (the load-time mirror of the
/// parameter/`let` shadowing `check_deferred_expr` already accounts for
/// on the deferred side).
fn reject_param_references(expr: &Expr, params: &[String], field: &str) -> Result<(), CoreError> {
    match expr {
        Expr::Bool(_) => Ok(()),
        Expr::Var(name) => {
            if params.iter().any(|p| p == &name.value) {
                Err(CoreError::new(
                    format!("beam parameters cannot be used in `{field}`"),
                    name.span,
                ))
            } else {
                Ok(())
            }
        }
        Expr::Str(template) => reject_param_references_in_template(template, params, field),
        Expr::Call { args, .. } => args
            .iter()
            .try_for_each(|arg| reject_param_references(arg, params, field)),
        Expr::Binary { lhs, rhs, .. } => {
            reject_param_references(lhs, params, field)?;
            reject_param_references(rhs, params, field)
        }
        Expr::If {
            cond,
            then,
            otherwise,
        } => {
            reject_param_references(cond, params, field)?;
            reject_param_references(then, params, field)?;
            reject_param_references(otherwise, params, field)
        }
        Expr::Field { .. } => Ok(()),
    }
}

fn reject_param_references_in_template(
    template: &StringTemplate,
    params: &[String],
    field: &str,
) -> Result<(), CoreError> {
    template.parts.iter().try_for_each(|part| match part {
        TemplatePart::Literal(_) => Ok(()),
        TemplatePart::Expr(e) => reject_param_references(e, params, field),
    })
}

/// Renders a load-time field's template, in three steps.
///
/// First, `params` may never appear in it (see the module doc comment),
/// checked explicitly so a same-named `let` can't silently mask a
/// forbidden parameter reference. Then the whole template is validated
/// without being evaluated — rendering alone would only walk the branch an
/// `if` condition happens to select, which is what would otherwise make a
/// name error here depend on the machine the file is loaded on. Only then
/// is it rendered, against `lets` alone.
fn render_load_time_template(
    field: &str,
    template: &StringTemplate,
    lets: &Scope,
    params: &[String],
) -> Result<String, CoreError> {
    reject_param_references_in_template(template, params, field)?;
    validate_template(template, lets, &[], Timing::Load)?;
    render_template(template, lets)
}

/// Converts a lexer/parser failure into a [`CoreError`], stamped with
/// whichever file is currently being loaded (see [`SourceIdScope`]).
/// Shared by [`load_str`] and [`crate::loader`], so a parse failure looks
/// identical regardless of whether it came from the single-file front door
/// or from a file loaded as part of an `import` chain.
pub(crate) fn parse_error_to_core_error(e: ParseError) -> CoreError {
    let err = CoreError::new(e.message, e.span);
    match e.help {
        Some(help) => err.with_help(help),
        None => err,
    }
}

/// Parses `source` as a single Beamfile and evaluates it into a
/// [`Project`]: file-level `let`s in order, then every `beam` declaration.
/// Single-file loading only — no `import` resolution, which is
/// [`crate::loader::load_project`]'s job.
///
/// `#[doc(hidden)]` rather than private: `loader`'s tests and the future
/// engine's tests reuse this exact helper (as `alba_core::load_str`) to
/// build a `Project` from an inline source string without needing a real
/// file on disk, but it isn't part of this crate's supported public API —
/// real callers go through `alba_core::load_project` instead.
///
/// Runs [`crate::graph::validate_graph`] before returning, exactly like
/// [`crate::loader::load_project`] does — so the engine's tests, which
/// build their `Project`s through this function, see the same unknown-
/// `needs`/cycle errors a real multi-file load would produce, rather than
/// the two entry points drifting apart on what counts as a valid graph.
///
/// Every beam this produces carries `SourceId(0)`, matching
/// [`crate::loader::load_project`]'s convention for the root file, and a
/// `dir` of `"."`. That second part is *not* the same convention
/// `load_project` uses: it resolves `Beam::dir` to an absolute path via
/// `std::path::absolute`, while this function has no real file on disk to
/// resolve `cwd` against, so it leaves `dir` as the literal, relative
/// `"."` instead.
///
/// `import` declarations are rejected outright rather than silently
/// ignored: this function has no filesystem to resolve them against
/// (there's no "importing file directory" for an inline string), so
/// silently dropping them would let a source string that accidentally
/// includes an `import` load successfully with beams quietly missing,
/// instead of failing loudly. Real `import` resolution is
/// `load_project`'s job.
#[doc(hidden)]
pub fn load_str(source: &str) -> Result<Project, CoreError> {
    let _scope = SourceIdScope::enter(ROOT_SOURCE_ID);
    let file = alba_syntax::parse(source).map_err(parse_error_to_core_error)?;
    if let Some(import) = file.imports.first() {
        return Err(CoreError::new(
            "imports are not supported by `load_str`; use `load_project` instead",
            import.path.span,
        ));
    }
    let project = build_project(
        &file,
        Path::new("."),
        Arc::new(LazyGit::new(PathBuf::from("."))),
    )?;
    crate::graph::validate_graph(&project)?;
    Ok(project)
}

/// Evaluates an already-parsed [`File`] into a [`Project`]: file-level
/// `let`s in order, then every `beam` declaration, each stamped with
/// `dir` and with whichever `SourceId` is currently active (see
/// [`SourceIdScope`]). Does not resolve `file.imports` — that recursion,
/// and the namespace prefixing it requires, belongs to
/// [`crate::loader::load_project`], which calls this once per file.
pub(crate) fn build_project(
    file: &File,
    dir: &Path,
    git: Arc<LazyGit>,
) -> Result<Project, CoreError> {
    let mut lets = Scope::with_git(git);
    for binding in &file.lets {
        // Validated before being evaluated, for the same reason a
        // load-time field is: evaluation only walks the branch an `if`
        // condition selects, so a name error hiding in the other one would
        // otherwise surface (or not) depending on the environment.
        let fallback = expr_span(&binding.value).unwrap_or(binding.name.span);
        check_expr(&binding.value, &lets, &[], Timing::Load, fallback)?;
        let value = eval_expr(&binding.value, &lets)?;
        lets.insert(binding.name.value.clone(), value);
    }

    let beams = file
        .beams
        .iter()
        .map(|decl| build_beam(decl, &lets, dir))
        .collect::<Result<Vec<_>, _>>()?;

    let default = file
        .default
        .as_ref()
        .map(|d| Spanned::new(BeamId(d.value.clone()), d.span));

    Ok(Project { beams, default })
}

fn beam_ref_to_id(r: &BeamRef) -> BeamId {
    if r.namespace.is_empty() {
        BeamId(r.name.clone())
    } else {
        BeamId(format!("{}:{}", r.namespace.join(":"), r.name))
    }
}

fn build_beam(decl: &BeamDecl, lets: &Scope, dir: &Path) -> Result<Beam, CoreError> {
    let params: Vec<String> = decl.params.iter().map(|p| p.value.clone()).collect();

    let description = decl
        .description
        .as_ref()
        .map(|t| render_load_time_template("description", t, lets, &params))
        .transpose()?;

    let needs = decl
        .needs
        .iter()
        .map(|n| Spanned::new(beam_ref_to_id(&n.value), n.span))
        .collect();

    let beam_name = decl.name.value.as_str();
    let inputs = build_patterns("inputs", beam_name, &decl.inputs, lets, &params)?;
    let outputs = build_patterns("outputs", beam_name, &decl.outputs, lets, &params)?;

    for run_template in &decl.run {
        validate_template(run_template, lets, &params, Timing::Schedule)?;
    }
    for (_, env_template) in &decl.env {
        validate_template(env_template, lets, &params, Timing::Schedule)?;
    }

    let cwd = decl
        .cwd
        .as_ref()
        .map(|t| render_load_time_template("cwd", t, lets, &params))
        .transpose()?;
    let executor = build_executor(decl.executor.as_ref(), lets, &params)?;

    Ok(Beam {
        id: BeamId(decl.name.value.clone()),
        description,
        needs,
        params,
        inputs,
        outputs,
        run: decl.run.clone(),
        env: decl
            .env
            .iter()
            .map(|(k, v)| (k.value.clone(), v.clone()))
            .collect(),
        cwd,
        executor,
        allow_failure: decl.allow_failure,
        dir: dir.to_path_buf(),
        span: decl.span,
        source: current_source_id(),
        scope: lets.clone(),
    })
}

/// Renders a beam's `inputs` or `outputs` templates and checks that each
/// result is a pattern the matcher can actually compile.
///
/// Compiled with `globset`, which is what [`crate::expand_globs`] matches
/// with — deliberately *not* `glob::Pattern`, the dialect the `glob()`
/// built-in validates against (see [`validate_glob_pattern`]). The two
/// accept different languages (`src**/x.rs` compiles under one and not the
/// other), so checking a pattern against the wrong one either rejects
/// something the matcher would have handled or lets through something it
/// cannot compile. The second is the damaging direction: an `inputs`
/// pattern that only fails at match time silently matches no file, and a
/// beam whose inputs match no file looks unchanged forever.
fn build_patterns(
    field: &str,
    beam: &str,
    templates: &[StringTemplate],
    lets: &Scope,
    params: &[String],
) -> Result<Vec<String>, CoreError> {
    let mut patterns = Vec::with_capacity(templates.len());
    for template in templates {
        let pattern = render_load_time_template(field, template, lets, params)?;
        if let Err(error) = globset::Glob::new(&pattern) {
            return Err(CoreError::new(
                format!(
                    "invalid `{field}` pattern `{pattern}` in beam `{beam}`: {}",
                    error.kind()
                ),
                template.span,
            ));
        }
        patterns.push(pattern);
    }
    Ok(patterns)
}

fn build_executor(
    decl: Option<&ExecutorDecl>,
    lets: &Scope,
    params: &[String],
) -> Result<ExecutorKind, CoreError> {
    let Some(decl) = decl else {
        return Ok(ExecutorKind::Shell);
    };

    // Render every option at load time, whatever the kind, so a malformed
    // interpolation anywhere in the block is still caught.
    let mut options: Vec<(String, Span, OptionValue)> = Vec::new();
    for (key, value) in &decl.options {
        let rendered = match value {
            ExecutorOptionValue::Str(template) => OptionValue::Str(render_load_time_template(
                "executor options",
                template,
                lets,
                params,
            )?),
            ExecutorOptionValue::Bool(flag) => OptionValue::Bool(*flag),
            ExecutorOptionValue::List(templates) => OptionValue::List(
                templates
                    .iter()
                    .map(|t| render_load_time_template("executor options", t, lets, params))
                    .collect::<Result<_, _>>()?,
            ),
        };
        options.push((key.value.clone(), key.span, rendered));
    }

    match decl.name.value.as_str() {
        "shell" => no_options("shell", &options, decl).map(|()| ExecutorKind::Shell),
        "system_shell" => {
            no_options("system_shell", &options, decl).map(|()| ExecutorKind::SystemShell)
        }
        "docker" => build_docker(decl, options),
        other => Ok(ExecutorKind::Plugin {
            name: other.to_string(),
            options: options.into_iter().map(|(k, _, v)| (k, v)).collect(),
        }),
    }
}

/// Rejects any option on an executor kind that takes none (`shell`,
/// `system_shell`).
fn no_options(
    name: &str,
    options: &[(String, Span, OptionValue)],
    decl: &ExecutorDecl,
) -> Result<(), CoreError> {
    if options.is_empty() {
        Ok(())
    } else {
        Err(CoreError::new(
            format!("executor `{name}` takes no options"),
            decl.name.span,
        ))
    }
}

/// Builds `ExecutorKind::Docker` from its already-rendered options:
/// `image` (required), `volumes` (a list of `host:container` entries), and
/// `workdir` (an absolute container path).
fn build_docker(
    decl: &ExecutorDecl,
    options: Vec<(String, Span, OptionValue)>,
) -> Result<ExecutorKind, CoreError> {
    let mut image = None;
    let mut volumes = Vec::new();
    let mut workdir = None;
    for (key, span, value) in options {
        match (key.as_str(), value) {
            ("image", OptionValue::Str(v)) => image = Some(v),
            ("image", _) => {
                return Err(CoreError::new(
                    "executor `docker` option `image` must be a string".to_string(),
                    span,
                ));
            }
            ("volumes", OptionValue::List(entries)) => {
                for entry in &entries {
                    // `rsplit_once` so a windows host path (`C:\cache`)
                    // keeps its drive colon; only the last `:` splits.
                    let valid = matches!(entry.rsplit_once(':'), Some((host, path))
                        if !host.is_empty() && path.starts_with('/'));
                    if !valid {
                        return Err(CoreError::new(
                            format!("invalid volume `{entry}`: expected `host:container`"),
                            span,
                        ));
                    }
                }
                volumes = entries;
            }
            ("volumes", _) => {
                return Err(CoreError::new(
                    "executor `docker` option `volumes` must be a list of strings".to_string(),
                    span,
                ));
            }
            ("workdir", OptionValue::Str(v)) if v.starts_with('/') => workdir = Some(v),
            ("workdir", _) => {
                return Err(CoreError::new(
                    "executor `docker` option `workdir` must be an absolute container path"
                        .to_string(),
                    span,
                ));
            }
            (other, _) => {
                let err = CoreError::new(
                    format!("executor `docker` does not take an option `{other}`"),
                    span,
                );
                return Err(
                    match suggest(other, ["image", "volumes", "workdir"].into_iter()) {
                        Some(c) => err.with_help(format!("did you mean `{c}`?")),
                        None => err,
                    },
                );
            }
        }
    }
    let image = image.ok_or_else(|| {
        CoreError::new(
            "executor `docker` requires an `image` option".to_string(),
            decl.name.span,
        )
    })?;
    Ok(ExecutorKind::Docker {
        image,
        volumes,
        workdir,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Serializes every test in this crate that touches the process
    /// environment. `libtest` runs test functions on several threads by
    /// default, and `setenv`/`unsetenv` reallocate the whole `environ`
    /// block, so two tests mutating *different* names still race each
    /// other. Held for a guard's entire lifetime, so a test's read of the
    /// variable it just set cannot be interleaved with another test's
    /// write.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Forces a variable to a known state for the duration of a test,
    /// regardless of what the ambient environment happens to hold, and
    /// restores whatever was there afterward — even if the test panics.
    /// Tests that read a variable through the real `env()` built-in would
    /// otherwise depend on whatever shell happens to run them.
    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
        /// Dropped last (after the restore in `Drop`, which runs before
        /// the struct's fields are dropped), so no other guard can take
        /// the lock until this one has put the variable back.
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        /// Ensures `name` is unset for this guard's lifetime.
        fn absent(name: &'static str) -> Self {
            Self::hold(name, None)
        }

        /// Ensures `name` holds `value` for this guard's lifetime.
        fn set(name: &'static str, value: &str) -> Self {
            Self::hold(name, Some(value))
        }

        fn hold(name: &'static str, value: Option<&str>) -> Self {
            // A poisoned lock means some earlier test panicked while
            // holding it; its own `Drop` still restored the variable, so
            // the environment is in a usable state and there is nothing to
            // recover. Taking the guard anyway keeps one failing test from
            // cascading into every other environment test.
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var(name).ok();
            // SAFETY: `set_var`/`remove_var` are `unsafe` because mutating
            // the process environment while another thread reads or writes
            // it is unsound. `ENV_LOCK` rules out the writer half
            // completely: it is the only path in this crate that mutates
            // the environment, and it is held here. The reader half is not
            // fully ruled out and cannot be from inside this crate —
            // `std` and other crates read the environment at moments we do
            // not control (`RUST_BACKTRACE` on a panic, `TMPDIR` from
            // `tempfile::tempdir()` in this crate's own graph tests), and
            // no lock we hold makes them wait. That residual risk is why
            // these calls remain `unsafe` rather than being wrapped away,
            // and it is the trade-off a test suite that must exercise
            // `env()` against the real environment accepts.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
            Self {
                name,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                // SAFETY: see `EnvVarGuard::hold`. Still under `ENV_LOCK`,
                // which this guard releases only once it has been dropped.
                Some(v) => unsafe { std::env::set_var(self.name, v) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    #[test]
    fn evaluates_let_chain_and_interpolation() {
        let _guard = EnvVarGuard::absent("ALBA_TEST_PROFILE");
        let src = r#"
let profile = env("ALBA_TEST_PROFILE", "debug")
let release = profile == "release"
beam build { run "cargo build {if release then '--release' else ''}" }
"#;
        let project = load_str(src).unwrap(); // test helper: parse + evaluate, single file
        let beam = &project.beams[0];
        let cmd = render_template(&beam.run[0], &beam.scope).unwrap();
        assert_eq!(cmd, "cargo build ");
    }

    /// The other half of `env()`'s load-time behaviour: when the variable
    /// *is* set, its value wins over the default.
    #[test]
    fn env_reads_a_variable_that_is_set() {
        let _guard = EnvVarGuard::set("ALBA_TEST_PROFILE", "release");
        let src = r#"
let profile = env("ALBA_TEST_PROFILE", "debug")
beam build { run "cargo build --{profile}" }
"#;
        let project = load_str(src).unwrap();
        let beam = &project.beams[0];

        let cmd = render_template(&beam.run[0], &beam.scope).unwrap();

        assert_eq!(cmd, "cargo build --release");
    }

    #[test]
    fn unknown_variable_reports_span_and_help() {
        let err = load_str(r#"beam x { run "{profil}" }"#).unwrap_err();
        assert!(err.message.contains("unknown variable"));
        // no `profil` in scope, nothing close: no help
        assert!(err.help.is_none());
    }

    #[test]
    fn if_condition_must_be_bool() {
        let err = load_str(
            r#"
let x = "str"
beam b { run "{if x then 'a' else 'b'}" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("expected a boolean"));
    }

    /// A beam parameter that shares a name with a file-level `let` shadows
    /// it (matching `Scope::with_params`'s documented shadowing), so
    /// load-time validation of a deferred `run` template must treat that
    /// name as unknown-until-schedule-time, not eagerly evaluate it using
    /// the (shadowed) `let`'s value. Concretely: `target` here is a
    /// `bool` `let`, but the beam's own `target` parameter shadows it and
    /// is always a string at schedule time, so `target + '!'` — string
    /// concatenation — must not be rejected as "`+` requires two
    /// strings, found a boolean and a string" at load time.
    #[test]
    fn beam_parameter_shadows_a_same_named_let() {
        let project = load_str(
            r#"
let target = true
beam deploy(target) { run "{target + '!'}" }
"#,
        )
        .unwrap();
        assert_eq!(project.beams[0].params, vec!["target".to_string()]);
    }

    /// The branch a statically-known `if` condition
    /// does *not* take must still be name-validated at load time — a
    /// typo hiding there must not depend on which machine (or which
    /// environment variable) the file happens to be loaded on.
    #[test]
    fn if_validates_the_not_taken_branch_too() {
        let err = load_str(
            r#"
let flag = false
beam b { run "{if flag then bogus else 'ok'}" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("unknown variable"));
        assert!(err.message.contains("bogus"));
    }

    /// A beam parameter is forbidden in load-time
    /// fields (`cwd` here) even when a same-named file-level `let`
    /// exists — the `let` must not silently stand in for it.
    #[test]
    fn load_time_field_rejects_parameter_even_when_shadowing_a_let() {
        let err = load_str(
            r#"
let target = "prod"
beam deploy(target) { cwd "{target}" run "echo {target}" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("beam parameters cannot be used"));
        assert!(err.message.contains("cwd"));
    }

    /// `env()` inside a deferred (`run`) template must
    /// not actually be executed at load time — only its name/arity are
    /// checked. A project with a beam referencing an unset environment
    /// variable in `run` must still load successfully; the "not set"
    /// failure belongs at schedule time, for whoever actually runs that
    /// beam.
    #[test]
    fn env_in_run_template_is_not_read_at_load_time() {
        let project =
            load_str(r#"beam deploy { run "curl -H {env('ALBA_PROBE_TOKEN_XYZ')}" }"#).unwrap();
        assert_eq!(project.beams[0].id.0, "deploy");
    }

    /// A load-time field is validated in full, not just along the branch a
    /// statically known condition happens to take. Which branch that is can
    /// depend on the environment the file is loaded in, so a typo hiding in
    /// the other one must not be reported on one machine and swallowed on
    /// the next.
    #[test]
    fn load_time_field_validates_the_not_taken_branch_too() {
        let err = load_str(
            r#"
let flag = false
beam b { cwd "{if flag then bogus else '.'}" run "echo x" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("unknown variable"));
        assert!(err.message.contains("bogus"));
    }

    /// The same rule for an unknown function hiding in an untaken branch of
    /// a load-time field.
    #[test]
    fn load_time_field_rejects_an_unknown_function_in_an_untaken_branch() {
        let err = load_str(
            r#"
let flag = false
beam b { description "{if flag then nope('x') else 'ok'}" run "echo x" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("unknown function"));
    }

    /// A `let` binding is a load-time expression too, so its untaken
    /// branches are validated the same way.
    #[test]
    fn let_binding_validates_the_not_taken_branch_too() {
        let err = load_str(
            r#"
let flag = false
let path = if flag then bogus else "."
beam b { run "echo {path}" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("unknown variable"));
        assert!(err.message.contains("bogus"));
    }

    /// `env()` without a default is only resolvable when a beam runs, so a
    /// load-time field must reject it at the call site rather than making
    /// the whole project's load depend on the ambient environment.
    #[test]
    fn env_without_a_default_is_rejected_in_a_load_time_field() {
        let err = load_str(r#"beam b { description "{env('ALBA_NOPE_XYZ')}" run "echo x" }"#)
            .unwrap_err();
        assert!(err.message.contains("requires a default"));
        assert!(err.help.is_some());
    }

    /// The same rule for a `let` binding, whatever the ambient environment
    /// holds — this is what makes `alba check` answer the same way on a
    /// laptop and in CI.
    #[test]
    fn env_without_a_default_is_rejected_in_a_let_binding() {
        let err = load_str(
            r#"
let x = env("ALBA_TEST_ABSENT_XYZ")
beam b { run "x" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("requires a default"));
    }

    /// `glob`'s arity is checked before its arguments are resolved, so a
    /// miscall is reported even when one argument only becomes known once
    /// a beam parameter is bound — matching how `env`'s arity is checked.
    #[test]
    fn glob_arity_is_checked_even_when_an_argument_is_unknown() {
        let err = load_str(r#"beam b(p) { run "{glob(p, 'x')}" }"#).unwrap_err();
        assert!(
            err.message
                .contains("`glob` expects 1 argument(s), found 2")
        );
    }

    #[test]
    fn glob_validates_pattern_and_returns_it_unchanged() {
        let project = load_str(
            r#"
let pattern = glob("src/**/*.rs")
beam b { run "{pattern}" }
"#,
        )
        .unwrap();
        assert_eq!(
            project.beams[0].scope.get("pattern"),
            Some(&Value::Str("src/**/*.rs".to_string()))
        );
    }

    #[test]
    fn glob_rejects_invalid_pattern() {
        let err = load_str(
            r#"
let pattern = glob("[unclosed")
beam b { run "x" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("invalid glob pattern"));
    }

    /// `inputs` patterns are matched with `globset` at run time, so they
    /// are compiled with `globset` here — a pattern that only fails there
    /// used to match nothing at all, which the cache read as a beam whose
    /// inputs never change.
    #[test]
    fn inputs_reject_a_pattern_that_will_not_compile() {
        let err = load_str(
            r#"
beam b {
  inputs ["src/**/*.rs", "a[b"]
  run "x"
}
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("invalid `inputs` pattern `a[b`"));
        assert!(err.message.contains("beam `b`"));
    }

    #[test]
    fn outputs_reject_a_pattern_that_will_not_compile() {
        let err = load_str(
            r#"
beam b {
  outputs ["a[b"]
  run "x"
}
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("invalid `outputs` pattern `a[b`"));
    }

    /// The `glob()` built-in and the `inputs`/`outputs` fields validate
    /// against two different, incompatible dialects — `glob::Pattern` for
    /// the former, `globset` for the latter, each being what actually
    /// consumes the pattern later. `src**/x.rs` is legal in one and not
    /// the other, and neither check may be swapped for the other's
    /// dialect: doing so would reject patterns the matcher accepts, or
    /// accept patterns it cannot compile.
    #[test]
    fn the_glob_builtin_and_the_inputs_field_keep_their_own_dialects() {
        let err = load_str(
            r#"
let pattern = glob("src**/x.rs")
beam b { run "{pattern}" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("invalid glob pattern `src**/x.rs`"));

        load_str(
            r#"
beam b {
  inputs ["src**/x.rs"]
  run "x"
}
"#,
        )
        .expect("`globset` accepts this, and `globset` is what matches `inputs`");
    }

    #[test]
    fn binary_operators_evaluate_correctly() {
        let project = load_str(
            r#"
let a = true
let b = false
let and_result = a && b
let or_result = a || b
let neq_result = a != b
let concat_result = "foo" + "bar"
beam x { run "ok" }
"#,
        )
        .unwrap();
        let scope = &project.beams[0].scope;
        assert_eq!(scope.get("and_result"), Some(&Value::Bool(false)));
        assert_eq!(scope.get("or_result"), Some(&Value::Bool(true)));
        assert_eq!(scope.get("neq_result"), Some(&Value::Bool(true)));
        assert_eq!(
            scope.get("concat_result"),
            Some(&Value::Str("foobar".to_string()))
        );
    }

    #[test]
    fn unknown_function_errors_with_help() {
        let err = load_str(
            r#"
let x = evn("Y")
beam b { run "x" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("unknown function"));
        assert_eq!(err.help.as_deref(), Some("did you mean `env`?"));
    }

    #[test]
    fn executor_docker_carries_image() {
        let project =
            load_str(r#"beam deploy { executor docker { image "registry/app:latest" } run "x" }"#)
                .unwrap();
        assert_eq!(
            project.beams[0].executor,
            ExecutorKind::Docker {
                image: "registry/app:latest".to_string(),
                volumes: Vec::new(),
                workdir: None,
            }
        );
    }

    #[test]
    fn executor_docker_carries_volumes_and_workdir() {
        let project = load_str(
            r#"beam d { executor docker { image "x" volumes ["h:/c"] workdir "/w" } run "x" }"#,
        )
        .unwrap();
        assert_eq!(
            project.beams[0].executor,
            ExecutorKind::Docker {
                image: "x".into(),
                volumes: vec!["h:/c".into()],
                workdir: Some("/w".into()),
            }
        );
    }

    #[test]
    fn docker_volumes_must_be_a_list() {
        let err = load_str(r#"beam d { executor docker { image "x" volumes "h:/c" } run "x" }"#)
            .unwrap_err();
        assert!(
            err.message
                .contains("executor `docker` option `volumes` must be a list of strings")
        );
    }

    #[test]
    fn docker_volume_entries_must_name_a_container_path() {
        let err =
            load_str(r#"beam d { executor docker { image "x" volumes ["nocolon"] } run "x" }"#)
                .unwrap_err();
        assert!(
            err.message
                .contains("invalid volume `nocolon`: expected `host:container`")
        );
    }

    #[test]
    fn docker_workdir_must_be_an_absolute_container_path() {
        let err =
            load_str(r#"beam d { executor docker { image "x" workdir "relative" } run "x" }"#)
                .unwrap_err();
        assert!(
            err.message
                .contains("executor `docker` option `workdir` must be an absolute container path")
        );
    }

    #[test]
    fn docker_rejects_an_unknown_option_with_a_suggestion() {
        let err = load_str(r#"beam d { executor docker { image "x" volume ["a:/b"] } run "x" }"#)
            .unwrap_err();
        assert!(
            err.message
                .contains("executor `docker` does not take an option `volume`")
        );
        assert_eq!(err.help.as_deref(), Some("did you mean `volumes`?"));
    }

    #[test]
    fn an_unknown_executor_becomes_a_plugin_reference() {
        let project = load_str(
            r#"beam d { executor podman { image "x" remote true tags ["a", "b"] } run "x" }"#,
        )
        .unwrap();
        assert_eq!(
            project.beams[0].executor,
            ExecutorKind::Plugin {
                name: "podman".into(),
                options: vec![
                    ("image".into(), OptionValue::Str("x".into())),
                    ("remote".into(), OptionValue::Bool(true)),
                    (
                        "tags".into(),
                        OptionValue::List(vec!["a".into(), "b".into()])
                    ),
                ],
            }
        );
    }

    #[test]
    fn shell_takes_no_options() {
        let err = load_str(r#"beam b { executor shell { image "x" } run "y" }"#).unwrap_err();
        assert!(err.message.contains("executor `shell` takes no options"));
    }

    /// A name close to `docker` (a plausible typo) is not special-cased:
    /// the load-time typo suggestion moved to plan time, so any name other
    /// than `shell`/`system_shell`/`docker` becomes a plugin reference,
    /// however near it reads to a known one.
    #[test]
    fn an_executor_name_near_docker_still_becomes_a_plugin_reference() {
        let project = load_str(r#"beam deploy { executor dokcer { image "x" } run "y" }"#).unwrap();
        assert_eq!(
            project.beams[0].executor,
            ExecutorKind::Plugin {
                name: "dokcer".to_string(),
                options: vec![("image".to_string(), OptionValue::Str("x".to_string()))],
            }
        );
    }

    #[test]
    fn system_shell_executor_maps_to_its_kind() {
        let project = load_str("beam b {\n  executor system_shell\n  run \"x\"\n}").unwrap();
        assert_eq!(project.beams[0].executor, ExecutorKind::SystemShell);
    }

    #[test]
    fn system_shell_rejects_options() {
        let err = load_str("beam b {\n  executor system_shell { image \"i\" }\n  run \"x\"\n}")
            .unwrap_err();
        assert!(
            err.message
                .contains("executor `system_shell` takes no options")
        );
    }

    /// Same rule as `an_executor_name_near_docker_still_becomes_a_plugin_reference`,
    /// for a typo of `system_shell`.
    #[test]
    fn an_executor_name_near_system_shell_still_becomes_a_plugin_reference() {
        let project = load_str("beam b {\n  executor system_shel\n  run \"x\"\n}").unwrap();
        assert_eq!(
            project.beams[0].executor,
            ExecutorKind::Plugin {
                name: "system_shel".to_string(),
                options: Vec::new(),
            }
        );
    }

    #[test]
    fn unknown_variable_suggests_close_match() {
        let err = load_str(
            r#"
let release = true
beam b { run "{relase}" }
"#,
        )
        .unwrap_err();
        assert_eq!(err.help.as_deref(), Some("did you mean `release`?"));
    }

    /// `suggest`'s candidates commonly come from
    /// `HashMap` iteration (`Scope::names`), whose order is randomized
    /// per process — without an explicit tie-break, two equally-close
    /// candidates could yield a different suggestion from run to run.
    #[test]
    fn suggest_breaks_distance_ties_alphabetically() {
        // "hat" is Levenshtein distance 1 from both "cat" and "bat".
        assert_eq!(suggest("hat", ["cat", "bat"].into_iter()), Some("bat"));
        assert_eq!(suggest("hat", ["bat", "cat"].into_iter()), Some("bat"));
    }

    /// `load_str` used to silently drop `import`
    /// declarations (it has no filesystem to resolve them against), which
    /// would let a source string containing one "succeed" with beams
    /// quietly missing. It must fail loudly instead.
    #[test]
    fn load_str_rejects_imports() {
        let err = load_str("import \"x/Beamfile\" as x\nbeam b { run \"echo\" }").unwrap_err();
        assert!(err.message.contains("imports are not supported"));
    }
}
