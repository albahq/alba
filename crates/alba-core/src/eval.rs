//! Expression evaluation, template rendering, and the [`Scope`] they run
//! against — plus the single-file `load_str` orchestration that walks a
//! parsed [`File`] into a [`Project`].
//!
//! ## Load-time vs. schedule-time rendering
//!
//! A beam's `description`, `inputs`, `outputs`, `cwd`, and executor
//! options are rendered once, here, at load time, using a [`Scope`] built
//! only from the file's `let` bindings: referencing a beam's own
//! parameter in one of these fields is therefore a plain "unknown
//! variable" error (the parameter was never added to that scope), which
//! satisfies the "parameters are forbidden here" rule without any special
//! casing.
//!
//! `run` and `env` values stay as [`StringTemplate`]s in the model,
//! rendered later (at schedule time, by the engine, through the same
//! [`render_template`] used here) against a [`Scope`] that also has the
//! beam's parameters bound via [`Scope::with_params`]. Because parameters
//! aren't bound yet at load time, those templates can't be fully
//! evaluated up front — but the brief still requires that every name they
//! reference resolve to *something* (a file-level `let` or one of the
//! beam's own parameters) at load time. [`validate_deferred_template`]
//! does that without evaluating: it walks the expression tree, treats a
//! reference to a file-level `let` as concretely known (since those are
//! already evaluated) and a reference to one of the beam's own parameters
//! as validly-named-but-unknown-until-schedule-time, and eagerly
//! type-checks (reusing [`eval_binary`]/[`eval_expr`]) any sub-expression
//! that turns out to depend only on `let`s. That is what makes
//! `if_condition_must_be_bool` (whose `if` condition is a plain `let`,
//! with no parameter involved) catchable at load time, while a `run`
//! template that mixes `let`s and parameters only gets its names checked,
//! not its types, until schedule time.

use std::collections::HashMap;
use std::path::PathBuf;

use alba_syntax::{
    BeamDecl, BeamRef, BinOp, ExecutorDecl, Expr, File, Span, Spanned, StringTemplate, TemplatePart,
};

use crate::error::CoreError;
use crate::model::{Beam, BeamId, ExecutorKind, Project, SourceId, Value};

/// The `SourceId` every error and beam produced by this crate's
/// single-file loading (`load_str`) carries. Multi-file loading (Task 7's
/// real `load_project` entry point) will assign distinct ids per imported
/// file instead of this constant.
pub(crate) const ROOT_SOURCE_ID: SourceId = SourceId(0);

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
}

impl Scope {
    /// An empty scope: no `let`s, no parameters.
    pub fn empty() -> Self {
        Self::default()
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
    pub fn with_params(&self, params: &[String], args: &[String]) -> Self {
        let mut values = self.values.clone();
        for (name, arg) in params.iter().zip(args.iter()) {
            values.insert(name.clone(), Value::Str(arg.clone()));
        }
        Self { values }
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
    }
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
/// distance 2 (picking the closest on ties by iteration order), matching
/// `alba_syntax`'s unknown-field suggestion threshold.
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .map(|candidate| (candidate, levenshtein(name, candidate)))
        .filter(|&(_, distance)| distance <= 2)
        .min_by_key(|&(_, distance)| distance)
        .map(|(candidate, _)| candidate)
}

fn unknown_variable_error<'a>(
    name: &Spanned<String>,
    candidates: impl Iterator<Item = &'a str>,
) -> CoreError {
    CoreError {
        message: format!("unknown variable `{}`", name.value),
        span: name.span,
        help: suggest(&name.value, candidates).map(|c| format!("did you mean `{c}`?")),
        source_id: ROOT_SOURCE_ID,
    }
}

fn unknown_function_error(name: &Spanned<String>) -> CoreError {
    CoreError {
        message: format!("unknown function `{}`", name.value),
        span: name.span,
        help: suggest(&name.value, BUILTIN_FUNCTIONS.iter().copied())
            .map(|c| format!("did you mean `{c}`?")),
        source_id: ROOT_SOURCE_ID,
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
                other => Err(CoreError {
                    message: format!(
                        "expected a boolean condition for `if`, found {}",
                        other.type_name()
                    ),
                    span: cond_span,
                    help: None,
                    source_id: ROOT_SOURCE_ID,
                }),
            }
        }
    }
}

fn eval_binary(op: BinOp, lhs: Value, rhs: Value, span: Span) -> Result<Value, CoreError> {
    match op {
        BinOp::Concat => match (lhs, rhs) {
            (Value::Str(a), Value::Str(b)) => Ok(Value::Str(a + &b)),
            (l, r) => Err(CoreError {
                message: format!(
                    "`+` requires two strings, found {} and {}",
                    l.type_name(),
                    r.type_name()
                ),
                span,
                help: None,
                source_id: ROOT_SOURCE_ID,
            }),
        },
        BinOp::Eq | BinOp::NotEq => {
            if std::mem::discriminant(&lhs) != std::mem::discriminant(&rhs) {
                return Err(CoreError {
                    message: format!(
                        "`==`/`!=` requires operands of the same type, found {} and {}",
                        lhs.type_name(),
                        rhs.type_name()
                    ),
                    span,
                    help: None,
                    source_id: ROOT_SOURCE_ID,
                });
            }
            let equal = lhs == rhs;
            Ok(Value::Bool(if op == BinOp::Eq { equal } else { !equal }))
        }
        BinOp::And | BinOp::Or => match (lhs, rhs) {
            (Value::Bool(a), Value::Bool(b)) => {
                Ok(Value::Bool(if op == BinOp::And { a && b } else { a || b }))
            }
            (l, r) => Err(CoreError {
                message: format!(
                    "`&&`/`||` requires two booleans, found {} and {}",
                    l.type_name(),
                    r.type_name()
                ),
                span,
                help: None,
                source_id: ROOT_SOURCE_ID,
            }),
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
    CoreError {
        message: format!(
            "`{}` expects {expected} argument(s), found {found}",
            name.value
        ),
        span: name.span,
        help: None,
        source_id: ROOT_SOURCE_ID,
    }
}

/// Evaluates `arg` and requires the result to be a string, for built-in
/// function arguments (`env`'s name/default, `glob`'s pattern) that are
/// never meaningfully anything else.
fn expect_str_arg(
    fn_name: &Spanned<String>,
    arg: &Expr,
    scope: &Scope,
    fallback: Span,
) -> Result<String, CoreError> {
    let span = expr_span(arg).unwrap_or(fallback);
    match eval_with_fallback(arg, scope, span)? {
        Value::Str(s) => Ok(s),
        other => Err(CoreError {
            message: format!(
                "argument to `{}` must be a string, found {}",
                fn_name.value,
                other.type_name()
            ),
            span,
            help: None,
            source_id: ROOT_SOURCE_ID,
        }),
    }
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
            None => Err(CoreError {
                message: format!("environment variable `{key}` is not set"),
                span: name.span,
                help: None,
                source_id: ROOT_SOURCE_ID,
            }),
        },
    }
}

/// `glob(pattern)`: validates that the pattern compiles (expansion is the
/// cache's job, later) and returns it unchanged as a string.
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
    glob::Pattern::new(&pattern).map_err(|e| CoreError {
        message: format!("invalid glob pattern `{pattern}`: {e}"),
        span: pattern_span,
        help: None,
        source_id: ROOT_SOURCE_ID,
    })?;
    Ok(Value::Str(pattern))
}

/// What [`check_deferred_expr`] statically knows about a sub-expression of
/// a deferred (`run`/`env`) template: either its concrete value (it only
/// touched file-level `let`s, which are already evaluated) or that it
/// depends on one of the beam's own parameters and so can't be known until
/// schedule time.
enum StaticValue {
    Known(Value),
    Unknown,
}

/// Walks `expr`, validating variable names and eagerly type-checking
/// whatever sub-expressions don't depend on `params`. See the module doc
/// comment for the overall strategy.
fn check_deferred_expr(
    expr: &Expr,
    lets: &Scope,
    params: &[String],
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
            let mut any_unknown = false;
            for part in &template.parts {
                if let TemplatePart::Expr(e) = part {
                    let span = expr_span(e).unwrap_or(template.span);
                    match check_deferred_expr(e, lets, params, span)? {
                        StaticValue::Known(_) => {}
                        StaticValue::Unknown => any_unknown = true,
                    }
                }
            }
            if any_unknown {
                Ok(StaticValue::Unknown)
            } else {
                Ok(StaticValue::Known(Value::Str(render_template(
                    template, lets,
                )?)))
            }
        }
        Expr::Call { name, args } => {
            if !BUILTIN_FUNCTIONS.contains(&name.value.as_str()) {
                return Err(unknown_function_error(name));
            }
            let mut any_unknown = false;
            for arg in args {
                let span = expr_span(arg).unwrap_or(fallback);
                match check_deferred_expr(arg, lets, params, span)? {
                    StaticValue::Known(_) => {}
                    StaticValue::Unknown => any_unknown = true,
                }
            }
            if any_unknown {
                Ok(StaticValue::Unknown)
            } else {
                // All arguments are known from `let`s alone: run the real
                // call (arity, env lookup / glob validation) now, reusing
                // `eval_expr` rather than duplicating that logic.
                Ok(StaticValue::Known(eval_expr(expr, lets)?))
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let l = check_deferred_expr(lhs, lets, params, expr_span(lhs).unwrap_or(fallback))?;
            let r = check_deferred_expr(rhs, lets, params, expr_span(rhs).unwrap_or(fallback))?;
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
            match check_deferred_expr(cond, lets, params, cond_span)? {
                StaticValue::Known(Value::Bool(true)) => {
                    check_deferred_expr(then, lets, params, expr_span(then).unwrap_or(fallback))
                }
                StaticValue::Known(Value::Bool(false)) => check_deferred_expr(
                    otherwise,
                    lets,
                    params,
                    expr_span(otherwise).unwrap_or(fallback),
                ),
                StaticValue::Known(other) => Err(CoreError {
                    message: format!(
                        "expected a boolean condition for `if`, found {}",
                        other.type_name()
                    ),
                    span: cond_span,
                    help: None,
                    source_id: ROOT_SOURCE_ID,
                }),
                StaticValue::Unknown => {
                    // Which branch runs isn't known until schedule time;
                    // still validate names/types in both.
                    check_deferred_expr(then, lets, params, expr_span(then).unwrap_or(fallback))?;
                    check_deferred_expr(
                        otherwise,
                        lets,
                        params,
                        expr_span(otherwise).unwrap_or(fallback),
                    )?;
                    Ok(StaticValue::Unknown)
                }
            }
        }
    }
}

/// Validates a deferred (`run`/`env`) template at load time without fully
/// evaluating it: every `Var` it references must be either a file-level
/// `let` or one of `params` (the beam's own parameters, unbound until
/// schedule time). See the module doc comment for why this also happens
/// to catch some type errors (like `if_condition_must_be_bool`) early.
fn validate_deferred_template(
    template: &StringTemplate,
    lets: &Scope,
    params: &[String],
) -> Result<(), CoreError> {
    for part in &template.parts {
        if let TemplatePart::Expr(e) = part {
            let span = expr_span(e).unwrap_or(template.span);
            check_deferred_expr(e, lets, params, span)?;
        }
    }
    Ok(())
}

/// Parses `source` as a single Beamfile and evaluates it into a
/// [`Project`]: file-level `let`s in order, then every `beam` declaration.
/// Single-file loading only (see the crate's module doc comments) — no
/// `import` resolution, which is Task 7's job.
///
/// `#[doc(hidden)]` rather than private: Task 7's loader tests and Task
/// 10's engine tests reuse this exact helper (as `alba_core::load_str`)
/// to build a `Project` from an inline source string without needing a
/// real file on disk, but it isn't part of this crate's supported public
/// API — real callers go through `alba_core::load_project` (Task 7)
/// instead.
#[doc(hidden)]
pub fn load_str(source: &str) -> Result<Project, CoreError> {
    let file = alba_syntax::parse(source).map_err(|e| CoreError {
        message: e.message,
        span: e.span,
        help: e.help,
        source_id: ROOT_SOURCE_ID,
    })?;
    build_project(&file)
}

fn build_project(file: &File) -> Result<Project, CoreError> {
    let mut lets = Scope::empty();
    for binding in &file.lets {
        let value = eval_expr(&binding.value, &lets)?;
        lets.insert(binding.name.value.clone(), value);
    }

    let beams = file
        .beams
        .iter()
        .map(|decl| build_beam(decl, &lets))
        .collect::<Result<Vec<_>, _>>()?;

    let default = file.default.as_ref().map(|d| BeamId(d.value.clone()));

    Ok(Project { beams, default })
}

fn beam_ref_to_id(r: &BeamRef) -> BeamId {
    match &r.namespace {
        Some(ns) => BeamId(format!("{ns}:{}", r.name)),
        None => BeamId(r.name.clone()),
    }
}

fn build_beam(decl: &BeamDecl, lets: &Scope) -> Result<Beam, CoreError> {
    let description = decl
        .description
        .as_ref()
        .map(|t| render_template(t, lets))
        .transpose()?;

    let needs = decl
        .needs
        .iter()
        .map(|n| beam_ref_to_id(&n.value))
        .collect();
    let params: Vec<String> = decl.params.iter().map(|p| p.value.clone()).collect();

    let inputs = decl
        .inputs
        .iter()
        .map(|t| render_template(t, lets))
        .collect::<Result<Vec<_>, _>>()?;
    let outputs = decl
        .outputs
        .iter()
        .map(|t| render_template(t, lets))
        .collect::<Result<Vec<_>, _>>()?;

    for run_template in &decl.run {
        validate_deferred_template(run_template, lets, &params)?;
    }
    for (_, env_template) in &decl.env {
        validate_deferred_template(env_template, lets, &params)?;
    }

    let cwd = decl
        .cwd
        .as_ref()
        .map(|t| render_template(t, lets))
        .transpose()?;
    let executor = build_executor(decl.executor.as_ref(), lets)?;

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
        // No real file on disk in single-file loading (Task 7 assigns
        // real directories once `import`s make one meaningful).
        dir: PathBuf::from("."),
        span: decl.span,
        source: ROOT_SOURCE_ID,
        scope: lets.clone(),
    })
}

fn build_executor(decl: Option<&ExecutorDecl>, lets: &Scope) -> Result<ExecutorKind, CoreError> {
    let Some(decl) = decl else {
        return Ok(ExecutorKind::Shell);
    };

    // Render every option at load time, even ones a given executor kind
    // doesn't end up using, so a malformed interpolation anywhere in the
    // block is still caught (matches "executor options" being a load-time
    // field per the brief).
    let mut options = HashMap::new();
    for (key, value) in &decl.options {
        options.insert(key.value.as_str(), render_template(value, lets)?);
    }

    match decl.name.value.as_str() {
        "shell" => Ok(ExecutorKind::Shell),
        "docker" => {
            let image = options.remove("image").ok_or_else(|| CoreError {
                message: "executor `docker` requires an `image` option".to_string(),
                span: decl.name.span,
                help: None,
                source_id: ROOT_SOURCE_ID,
            })?;
            Ok(ExecutorKind::Docker { image })
        }
        other => Err(CoreError {
            message: format!("unknown executor `{other}`"),
            span: decl.name.span,
            help: suggest(other, ["shell", "docker"].into_iter())
                .map(|c| format!("did you mean `{c}`?")),
            source_id: ROOT_SOURCE_ID,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensures `ALBA_TEST_PROFILE` is absent for the duration of a test,
    /// regardless of what the ambient environment happens to hold, and
    /// restores whatever was there afterward — even if the test panics.
    /// `evaluates_let_chain_and_interpolation` reads this variable through
    /// the real `env()` built-in and asserts the *absent* branch; without
    /// this guard the test's outcome would depend on whatever shell
    /// happens to run it (the exact trap the task brief calls out).
    struct EnvVarGuard {
        name: &'static str,
        previous: Option<String>,
    }

    impl EnvVarGuard {
        fn absent(name: &'static str) -> Self {
            let previous = std::env::var(name).ok();
            // SAFETY: this is the only test in this crate that touches
            // process environment variables, and this crate spawns no
            // other threads that read or write them, so there is no
            // concurrent access for `set_var`/`remove_var`'s documented
            // hazard to apply to.
            unsafe { std::env::remove_var(name) };
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                // SAFETY: see `EnvVarGuard::absent`.
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
}
