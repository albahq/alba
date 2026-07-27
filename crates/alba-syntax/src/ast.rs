//! Typed AST for the Beamfile DSL, produced by [`crate::parser::parse`].
//!
//! Every piece of source-derived data is wrapped in [`Spanned<T>`] so
//! diagnostics can point a caret at the exact offending text rather than at
//! an enclosing construct.
//!
//! String-shaped beam fields (`description`, `inputs`, `outputs`, `run`,
//! `env` values, `cwd`, executor option values) are [`StringTemplate`]s:
//! every string in the AST supports `{expr}` interpolation. Identifiers
//! that name things rather than hold interpolatable text (`import` paths,
//! the `as` alias, `version`, `default`, beam names, parameter names) stay
//! plain `Spanned<String>`.

use crate::expr::Expr;
use crate::template::StringTemplate;
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
/// entry in an `executor { ... }` block: a name paired with its templated
/// string value. Named to keep clippy's `type_complexity` lint quiet.
pub type NamedString = (Spanned<String>, StringTemplate);

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
    pub description: Option<StringTemplate>,
    pub needs: Vec<Spanned<BeamRef>>,
    pub inputs: Vec<StringTemplate>,
    pub outputs: Vec<StringTemplate>,
    /// One entry for the single-string form of `run`, N entries for the
    /// list form.
    pub run: Vec<StringTemplate>,
    pub env: Vec<NamedString>,
    pub cwd: Option<StringTemplate>,
    pub executor: Option<ExecutorDecl>,
    pub allow_failure: bool,
    pub span: Span,
}
