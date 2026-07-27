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
//! evaluated up front — but the brief still requires that every name they
//! reference resolve to *something* (a file-level `let` or one of the
//! beam's own parameters) at load time, in *every* branch of the
//! expression (including one an `if` with a statically known condition
//! doesn't take — a typo hiding in a dead branch must still be caught).
//! [`validate_deferred_template`] does that without evaluating: it walks
//! the expression tree via [`check_deferred_expr`], treating a reference
//! to a file-level `let` as concretely known (since those are already
//! evaluated; a same-named parameter still shadows it, matching
//! [`Scope::with_params`]) and a reference to one of the beam's own
//! parameters as validly-named-but-unknown-until-schedule-time, and
//! eagerly type-checks (reusing [`eval_binary`]) any sub-expression that
//! turns out to depend only on `let`s. That is what makes
//! `if_condition_must_be_bool` (whose `if` condition is a plain `let`,
//! with no parameter involved) catchable at load time, while a `run`
//! template that mixes `let`s and parameters only gets its names checked,
//! not its types, until schedule time.
//!
//! One built-in is deliberately never executed on this deferred path:
//! `env()` reads the process environment, a side effect that belongs at
//! schedule time even when its arguments happen to be fully known already
//! — see `check_deferred_expr`'s `Call` arm. `glob()` has no such side
//! effect (it only compiles a pattern), so it stays eagerly validated.

use std::collections::HashMap;
use std::path::Path;

use alba_syntax::{
    BeamDecl, BeamRef, BinOp, ExecutorDecl, Expr, File, ParseError, Span, Spanned, StringTemplate,
    TemplatePart,
};

use crate::error::{CoreError, ROOT_SOURCE_ID, SourceIdScope, current_source_id};
use crate::model::{Beam, BeamId, ExecutorKind, Project, Value};

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
    ///
    /// Mismatched lengths are not validated here: `zip` simply stops at
    /// the shorter of the two, so a missing argument leaves its parameter
    /// name unbound (rendering later fails with a plain "unknown
    /// variable" error, not an arity message) and an extra argument is
    /// silently ignored. Task 10 is expected to check `args.len() ==
    /// beam.params.len()` itself before calling this, so a real
    /// argument-count mismatch gets a clear diagnostic instead of
    /// surfacing here as a confusing unknown-variable one.
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
/// distance 2, matching `alba_syntax`'s unknown-field suggestion
/// threshold. Ties are broken alphabetically, not by iteration order:
/// `candidates` is typically a `HashMap`'s keys (via `Scope::names`),
/// whose iteration order is randomized per process, so without an
/// explicit tie-break the suggestion offered for the same typo could
/// change from run to run.
fn suggest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
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
/// comment for the overall strategy — in particular, *every* branch of an
/// `if` is validated here, even one a statically known condition doesn't
/// take, and `env()` is never actually executed on this path.
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
                        match check_deferred_expr(e, lets, params, span)? {
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
        Expr::Call { name, args } => {
            if !BUILTIN_FUNCTIONS.contains(&name.value.as_str()) {
                return Err(unknown_function_error(name));
            }

            if name.value == "env" {
                // Reading the environment is a side effect that belongs
                // at schedule time, not load time (see the module doc
                // comment): check the call's arity and the names inside
                // its arguments, but never execute the read — not even
                // when every argument turns out to be statically known.
                // Otherwise a `run` template referencing an unset
                // variable would fail the *entire project's* load, for a
                // beam nobody asked to run.
                if args.is_empty() || args.len() > 2 {
                    return Err(arity_error(name, 1, 2, args.len()));
                }
                for arg in args {
                    let span = expr_span(arg).unwrap_or(fallback);
                    check_deferred_expr(arg, lets, params, span)?;
                }
                return Ok(StaticValue::Unknown);
            }

            // Only "glob" remains, and it has no side effect (it only
            // compiles the pattern), so it's safe — and required, per the
            // brief's "type errors ... unknown function" list — to
            // validate it eagerly once every argument is known. Reuses
            // each argument's already-computed `StaticValue` instead of
            // evaluating the call a second time via `eval_expr`.
            let mut arg_values = Vec::with_capacity(args.len());
            let mut any_unknown = false;
            for arg in args {
                let span = expr_span(arg).unwrap_or(fallback);
                match check_deferred_expr(arg, lets, params, span)? {
                    StaticValue::Known(v) => arg_values.push(v),
                    StaticValue::Unknown => any_unknown = true,
                }
            }
            if any_unknown {
                return Ok(StaticValue::Unknown);
            }
            if args.len() != 1 {
                return Err(arity_error(name, 1, 1, args.len()));
            }
            let pattern_span = expr_span(&args[0]).unwrap_or(fallback);
            let pattern = expect_str_value("glob", arg_values.remove(0), pattern_span)?;
            Ok(StaticValue::Known(validate_glob_pattern(
                pattern,
                pattern_span,
            )?))
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
            let then_span = expr_span(then).unwrap_or(fallback);
            let otherwise_span = expr_span(otherwise).unwrap_or(fallback);
            match check_deferred_expr(cond, lets, params, cond_span)? {
                StaticValue::Known(Value::Bool(cond_value)) => {
                    // Both branches are validated regardless of which one
                    // the condition picks: an untaken branch can still
                    // reference a nonexistent name, and which branch is
                    // "untaken" can depend on the environment the file
                    // happens to be loaded in (e.g. a condition derived
                    // from `env(...)`) — that must not make the
                    // diagnostic environment-dependent too.
                    let then_result = check_deferred_expr(then, lets, params, then_span)?;
                    let else_result = check_deferred_expr(otherwise, lets, params, otherwise_span)?;
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
                    // Which branch runs isn't known until schedule time;
                    // still validate names/types in both.
                    check_deferred_expr(then, lets, params, then_span)?;
                    check_deferred_expr(otherwise, lets, params, otherwise_span)?;
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

/// Renders a load-time field's template: `params` may never appear in it
/// (see the module doc comment), checked explicitly first so a
/// same-named `let` can't silently mask a forbidden parameter reference;
/// then rendered against `lets` alone.
fn render_load_time_template(
    field: &str,
    template: &StringTemplate,
    lets: &Scope,
    params: &[String],
) -> Result<String, CoreError> {
    reject_param_references_in_template(template, params, field)?;
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
/// Every beam this produces carries `SourceId(0)` and a `dir` of `"."`,
/// matching [`crate::loader::load_project`]'s convention for the root file
/// but without a real file on disk to resolve `cwd` against.
#[doc(hidden)]
pub fn load_str(source: &str) -> Result<Project, CoreError> {
    let _scope = SourceIdScope::enter(ROOT_SOURCE_ID);
    let file = alba_syntax::parse(source).map_err(parse_error_to_core_error)?;
    build_project(&file, Path::new("."))
}

/// Evaluates an already-parsed [`File`] into a [`Project`]: file-level
/// `let`s in order, then every `beam` declaration, each stamped with
/// `dir` and with whichever `SourceId` is currently active (see
/// [`SourceIdScope`]). Does not resolve `file.imports` — that recursion,
/// and the namespace prefixing it requires, belongs to
/// [`crate::loader::load_project`], which calls this once per file.
pub(crate) fn build_project(file: &File, dir: &Path) -> Result<Project, CoreError> {
    let mut lets = Scope::empty();
    for binding in &file.lets {
        let value = eval_expr(&binding.value, &lets)?;
        lets.insert(binding.name.value.clone(), value);
    }

    let beams = file
        .beams
        .iter()
        .map(|decl| build_beam(decl, &lets, dir))
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
        .map(|n| beam_ref_to_id(&n.value))
        .collect();

    let inputs = decl
        .inputs
        .iter()
        .map(|t| render_load_time_template("inputs", t, lets, &params))
        .collect::<Result<Vec<_>, _>>()?;
    let outputs = decl
        .outputs
        .iter()
        .map(|t| render_load_time_template("outputs", t, lets, &params))
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

fn build_executor(
    decl: Option<&ExecutorDecl>,
    lets: &Scope,
    params: &[String],
) -> Result<ExecutorKind, CoreError> {
    let Some(decl) = decl else {
        return Ok(ExecutorKind::Shell);
    };

    // Render every option at load time, even ones a given executor kind
    // doesn't end up using, so a malformed interpolation anywhere in the
    // block is still caught (matches "executor options" being a load-time
    // field per the brief).
    let mut options = HashMap::new();
    for (key, value) in &decl.options {
        options.insert(
            key.value.as_str(),
            render_load_time_template("executor options", value, lets, params)?,
        );
    }

    match decl.name.value.as_str() {
        "shell" => Ok(ExecutorKind::Shell),
        "docker" => {
            let image = options.remove("image").ok_or_else(|| {
                CoreError::new(
                    "executor `docker` requires an `image` option".to_string(),
                    decl.name.span,
                )
            })?;
            Ok(ExecutorKind::Docker { image })
        }
        other => {
            let err = CoreError::new(format!("unknown executor `{other}`"), decl.name.span);
            Err(match suggest(other, ["shell", "docker"].into_iter()) {
                Some(c) => err.with_help(format!("did you mean `{c}`?")),
                None => err,
            })
        }
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
            // SAFETY: `set_var`/`remove_var` are `unsafe` because
            // concurrently mutating and reading the process environment
            // from different threads is unsound, and this test suite does
            // not fully rule that out — `cargo test` runs test functions
            // on multiple threads by default, and both the standard
            // library and other crates can read the environment at any
            // time (e.g. `RUST_BACKTRACE` on a panic). What *is* true,
            // and is the actual basis for accepting this: no other test
            // in this crate calls `set_var`/`remove_var`, so nothing here
            // races another *write*, and `ALBA_TEST_PROFILE` is a name no
            // other code in this process has any reason to read. This is
            // the same trade-off most Rust test suites that need to touch
            // environment variables accept, rather than eliminate.
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

    /// Review finding #1: the branch a statically-known `if` condition
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

    /// Review finding #2: a beam parameter is forbidden in load-time
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

    /// Review finding #3: `env()` inside a deferred (`run`) template must
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

    #[test]
    fn env_without_default_errors_when_absent() {
        let _guard = EnvVarGuard::absent("ALBA_TEST_ABSENT_XYZ");
        let err = load_str(
            r#"
let x = env("ALBA_TEST_ABSENT_XYZ")
beam b { run "x" }
"#,
        )
        .unwrap_err();
        assert!(err.message.contains("is not set"));
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
                image: "registry/app:latest".to_string()
            }
        );
    }

    #[test]
    fn unknown_executor_errors_with_help() {
        let err = load_str(r#"beam deploy { executor dokcer { image "x" } run "y" }"#).unwrap_err();
        assert!(err.message.contains("unknown executor"));
        assert_eq!(err.help.as_deref(), Some("did you mean `docker`?"));
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

    /// Review minor #5: `suggest`'s candidates commonly come from
    /// `HashMap` iteration (`Scope::names`), whose order is randomized
    /// per process — without an explicit tie-break, two equally-close
    /// candidates could yield a different suggestion from run to run.
    #[test]
    fn suggest_breaks_distance_ties_alphabetically() {
        // "hat" is Levenshtein distance 1 from both "cat" and "bat".
        assert_eq!(suggest("hat", ["cat", "bat"].into_iter()), Some("bat"));
        assert_eq!(suggest("hat", ["bat", "cat"].into_iter()), Some("bat"));
    }
}
