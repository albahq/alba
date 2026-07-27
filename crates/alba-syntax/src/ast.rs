//! Typed AST for the Beamfile DSL, produced by [`crate::parser::parse`].
//!
//! Every piece of source-derived data is wrapped in [`Spanned<T>`] so
//! diagnostics can point a caret at the exact offending text rather than at
//! an enclosing construct.
//!
//! String-shaped beam fields (`description`, `inputs`, `outputs`, `run`,
//! `env` values, `cwd`, executor option values) are stored as raw
//! `Spanned<String>` for now. Task 4 introduces `StringTemplate` (parsed
//! string interpolation) and swaps it in for those fields; this task only
//! needs to preserve the raw text and its span.

use crate::token::Span;

/// A value together with the span of source text it was parsed from.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub value: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }
}

/// A fully parsed Beamfile.
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub version: Option<Spanned<String>>,
    pub imports: Vec<Import>,
    pub lets: Vec<LetBinding>,
    pub default: Option<Spanned<String>>,
    pub beams: Vec<BeamDecl>,
}

/// `import "path" as alias`.
#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub path: Spanned<String>,
    pub alias: Spanned<String>,
}

/// `let name = value`.
#[derive(Debug, Clone, PartialEq)]
pub struct LetBinding {
    pub name: Spanned<String>,
    pub value: Expr,
}

/// A reference to a beam in a `needs [...]` list, optionally namespaced by
/// an import alias (`api:build` vs. plain `codegen`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeamRef {
    pub namespace: Option<String>,
    pub name: String,
}

/// A `NAME = value` entry in an `env { ... }` block, or an `option "value"`
/// entry in an `executor { ... }` block: a name paired with its raw string
/// value. Named to keep clippy's `type_complexity` lint quiet and to give
/// Task 4 a single place to retarget the value half at `StringTemplate`.
pub type NamedString = (Spanned<String>, Spanned<String>);

/// `executor <name> { option "value", ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorDecl {
    pub name: Spanned<String>,
    pub options: Vec<NamedString>,
}

/// `beam name(params) { ... }`.
#[derive(Debug, Clone, PartialEq)]
pub struct BeamDecl {
    pub name: Spanned<String>,
    pub params: Vec<Spanned<String>>,
    pub description: Option<Spanned<String>>,
    pub needs: Vec<Spanned<BeamRef>>,
    pub inputs: Vec<Spanned<String>>,
    pub outputs: Vec<Spanned<String>>,
    /// One entry for the single-string form of `run`, N entries for the
    /// list form.
    pub run: Vec<Spanned<String>>,
    pub env: Vec<NamedString>,
    pub cwd: Option<Spanned<String>>,
    pub executor: Option<ExecutorDecl>,
    pub allow_failure: bool,
    pub span: Span,
}

/// An expression, as used in `let` bindings and `env` block values.
///
/// This is deliberately minimal: only what `let profile = env("PROFILE",
/// "debug")`, `let release = profile == "release"`, string literals,
/// booleans, and identifiers need. Task 4 adds `If`/`Concat`/`And`/`Or` and
/// a full precedence-climbing parser; the shape here (in particular `Str`
/// holding a raw `Spanned<String>` instead of a `StringTemplate`, and
/// `Bool`/`Call`/`Binary` matching Task 4's target shape already) is chosen
/// so that extension is additive rather than a rewrite.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Str(Spanned<String>),
    Bool(bool),
    Var(Spanned<String>),
    Call {
        name: Spanned<String>,
        args: Vec<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
}

/// A binary operator usable in an [`Expr::Binary`].
///
/// Only `Eq` is needed (and parsed) by this task; Task 4's Pratt parser
/// adds `NotEq`, `And`, `Or`, and string concatenation (`+`) alongside the
/// variants they need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
}
