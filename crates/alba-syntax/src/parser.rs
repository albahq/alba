//! Recursive descent parser turning a token stream into a [`crate::ast::File`].
//!
//! Grammar notes not obvious from the AST shapes alone:
//!
//! - The Beamfile DSL is brace/bracket/paren-delimited with no significant
//!   whitespace (see the design spec). `Newline` tokens from the lexer are
//!   therefore dropped up front, before the parser ever sees them: they
//!   carry no grammatical meaning here, so a `needs` list (or any other
//!   list) may freely span multiple lines, and beam fields are separated
//!   purely by the fact that each one starts with a recognizable field
//!   keyword-ish identifier.
//! - A field may appear at most once per `beam { ... }` block; a repeat is
//!   a parse error (`duplicate field`). Not spelled out by the grammar
//!   example, but the safer reading: silently accepting (and presumably
//!   overwriting or merging) a repeated field is far more likely to hide a
//!   copy-paste mistake than to be an intentional pattern.
//! - `needs [...]`/`inputs [...]`/`outputs [...]`/list-form `run [...]`
//!   accept an optional trailing comma before `]`.
//! - `env { NAME = value }` values and `executor <name> { option value }`
//!   option values are not shown as syntax by the brief's struct
//!   definitions; modeled on the design spec's example. `executor` options
//!   follow the same "keyword-ish identifier followed by its value" shape
//!   as beam fields (`image "deployer:latest"`, no `=`), and their value is
//!   always a string literal, parsed as a `StringTemplate`. `env` values
//!   may be a string literal (also a `StringTemplate`) or a bare identifier
//!   (a reference to a `let` binding or a beam parameter, as in
//!   `env { DEPLOY_TARGET = target }`); the bare-identifier form is
//!   modeled as a single-part template wrapping that variable reference —
//!   equivalent to writing `"{target}"` — so `NamedString`'s value half
//!   stays uniformly a `StringTemplate` either way.

use crate::ast::{BeamDecl, BeamRef, ExecutorDecl, File, Import, LetBinding, NamedString, Spanned};
use crate::expr::Expr;
use crate::lexer::Lexer;
use crate::template::{StringTemplate, TemplatePart};
use crate::token::{Span, Token, TokenKind};
use std::collections::HashSet;

/// The field names recognized inside a `beam { ... }` block.
const BEAM_FIELDS: &[&str] = &[
    "description",
    "needs",
    "inputs",
    "outputs",
    "run",
    "env",
    "cwd",
    "executor",
    "allow_failure",
];

/// An error produced while parsing a token stream (including lexing
/// errors, surfaced through the same type since callers only ever see
/// [`parse`]'s single `Result`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
}

/// Parses a whole Beamfile from source text into a typed [`File`].
pub fn parse(source: &str) -> Result<File, ParseError> {
    let tokens = tokenize(source)?;
    Parser::new(tokens, source).parse_file()
}

/// Lexes `source` into a flat token list, dropping `Newline` tokens (see
/// the module doc comment) and stopping at the first lex error.
///
/// `pub(crate)` so `expr.rs`'s tests and `template.rs`'s interpolation
/// handling can tokenize source text the same way the top-level parser
/// does.
pub(crate) fn tokenize(source: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    for result in Lexer::new(source) {
        match result {
            Ok(token) => {
                if token.kind != TokenKind::Newline {
                    tokens.push(token);
                }
            }
            Err(err) => {
                return Err(ParseError {
                    message: err.message,
                    span: err.span,
                    help: None,
                });
            }
        }
    }
    Ok(tokens)
}

/// Per-beam field accumulator, threaded through [`Parser::parse_beam_field`]
/// so that function doesn't need ten separate `&mut` parameters.
#[derive(Default)]
struct BeamFields {
    description: Option<StringTemplate>,
    needs: Vec<Spanned<BeamRef>>,
    inputs: Vec<StringTemplate>,
    outputs: Vec<StringTemplate>,
    run: Vec<StringTemplate>,
    env: Vec<NamedString>,
    cwd: Option<StringTemplate>,
    executor: Option<ExecutorDecl>,
    allow_failure: bool,
    seen: HashSet<String>,
}

/// Recursive descent parser over a flat token stream, plus (in `expr.rs`,
/// an `impl` block for this same type) the precedence-climbing expression
/// parser. Keeps `source` alongside the tokens because interpolation
/// (`template.rs`) needs to re-slice and re-lex the *original* source text
/// to keep its spans accurate; see `template.rs`'s module doc comment.
pub(crate) struct Parser<'a> {
    tokens: Vec<Token>,
    source: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(tokens: Vec<Token>, source: &'a str) -> Self {
        Self {
            tokens,
            source,
            pos: 0,
        }
    }

    /// The full original source text this parser's tokens were lexed from,
    /// for slicing out the raw text behind a token's span.
    pub(crate) fn source(&self) -> &'a str {
        self.source
    }

    pub(crate) fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    pub(crate) fn is_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    /// Consumes and returns the current token. Never advances past the
    /// trailing `Eof`, so repeated calls once at end of input keep
    /// returning `Eof` rather than panicking.
    pub(crate) fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    pub(crate) fn check(&self, kind: &TokenKind) -> bool {
        &self.peek().kind == kind
    }

    pub(crate) fn expect(&mut self, kind: TokenKind, what: &str) -> Result<Token, ParseError> {
        if self.check(&kind) {
            Ok(self.advance())
        } else {
            Err(self.unexpected(what))
        }
    }

    pub(crate) fn unexpected(&self, expected: &str) -> ParseError {
        let tok = self.peek();
        ParseError {
            message: format!("expected {expected}, found {}", describe(&tok.kind)),
            span: tok.span,
            help: None,
        }
    }

    pub(crate) fn eat_ident(&mut self) -> Result<Spanned<String>, ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Ident(name) => {
                let span = self.advance().span;
                Ok(Spanned::new(name, span))
            }
            _ => Err(self.unexpected("an identifier")),
        }
    }

    fn eat_str(&mut self) -> Result<Spanned<String>, ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Str(s) => {
                let span = self.advance().span;
                Ok(Spanned::new(s, span))
            }
            _ => Err(self.unexpected("a string literal")),
        }
    }

    /// Consumes a string literal and parses it into a [`StringTemplate`],
    /// resolving `{expr}` interpolation. Used for every AST field that
    /// holds interpolatable text (as opposed to `eat_str`, still used for
    /// plain identifiers-as-text like `import` paths and `version`).
    pub(crate) fn eat_template(&mut self) -> Result<StringTemplate, ParseError> {
        if matches!(self.peek().kind, TokenKind::Str(_)) {
            let tok = self.advance();
            crate::template::parse_template_at(self.source, tok.span)
        } else {
            Err(self.unexpected("a string literal"))
        }
    }

    /// Parses a `[item, item, ...]` list, with `parse_item` consuming a
    /// single element. Accepts an optional trailing comma.
    fn parse_bracketed_list<T>(
        &mut self,
        mut parse_item: impl FnMut(&mut Self) -> Result<T, ParseError>,
    ) -> Result<Vec<T>, ParseError> {
        self.expect(TokenKind::LBracket, "`[`")?;
        let mut items = Vec::new();
        if !self.check(&TokenKind::RBracket) {
            loop {
                items.push(parse_item(self)?);
                if self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RBracket) {
                        break; // trailing comma
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RBracket, "`]`")?;
        Ok(items)
    }

    fn parse_template_list(&mut self) -> Result<Vec<StringTemplate>, ParseError> {
        self.parse_bracketed_list(Self::eat_template)
    }

    fn parse_beam_ref(&mut self) -> Result<Spanned<BeamRef>, ParseError> {
        let first = self.eat_ident()?;
        if self.check(&TokenKind::Colon) {
            self.advance();
            let name = self.eat_ident()?;
            let span = Span::new(first.span.start, name.span.end);
            Ok(Spanned::new(
                BeamRef {
                    namespace: Some(first.value),
                    name: name.value,
                },
                span,
            ))
        } else {
            let span = first.span;
            Ok(Spanned::new(
                BeamRef {
                    namespace: None,
                    name: first.value,
                },
                span,
            ))
        }
    }

    fn parse_needs_list(&mut self) -> Result<Vec<Spanned<BeamRef>>, ParseError> {
        self.parse_bracketed_list(Self::parse_beam_ref)
    }

    /// `run "cmd"` (single command) or `run ["cmd", "cmd"]` (sequential list).
    fn parse_run_value(&mut self) -> Result<Vec<StringTemplate>, ParseError> {
        if self.check(&TokenKind::LBracket) {
            self.parse_template_list()
        } else {
            Ok(vec![self.eat_template()?])
        }
    }

    fn parse_bool_value(&mut self) -> Result<bool, ParseError> {
        match self.peek().kind {
            TokenKind::KwTrue => {
                self.advance();
                Ok(true)
            }
            TokenKind::KwFalse => {
                self.advance();
                Ok(false)
            }
            _ => Err(self.unexpected("`true` or `false`")),
        }
    }

    /// A string literal (parsed as a [`StringTemplate`]) or a bare
    /// identifier — a variable reference, modeled as a single-part
    /// template wrapping that reference. See the module doc comment.
    fn parse_env_value(&mut self) -> Result<StringTemplate, ParseError> {
        match self.peek().kind.clone() {
            TokenKind::Str(_) => self.eat_template(),
            TokenKind::Ident(name) => {
                let span = self.advance().span;
                Ok(StringTemplate {
                    parts: vec![TemplatePart::Expr(Expr::Var(Spanned::new(name, span)))],
                    span,
                })
            }
            _ => Err(self.unexpected("a string or an identifier")),
        }
    }

    fn parse_env_block(&mut self) -> Result<Vec<NamedString>, ParseError> {
        self.expect(TokenKind::LBrace, "`{`")?;
        let mut entries = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let key = self.eat_ident()?;
            self.expect(TokenKind::Eq, "`=`")?;
            let value = self.parse_env_value()?;
            entries.push((key, value));
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }
        self.expect(TokenKind::RBrace, "`}`")?;
        Ok(entries)
    }

    fn parse_executor_decl(&mut self) -> Result<ExecutorDecl, ParseError> {
        let name = self.eat_ident()?;
        self.expect(TokenKind::LBrace, "`{`")?;
        let mut options = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let opt_name = self.eat_ident()?;
            let opt_value = self.eat_template()?;
            options.push((opt_name, opt_value));
        }
        self.expect(TokenKind::RBrace, "`}`")?;
        Ok(ExecutorDecl { name, options })
    }

    /// Parses a single `field value` pair inside a `beam { ... }` block,
    /// dispatching on the field's identifier and folding the result into
    /// `fields`. Rejects unknown field names (with a Levenshtein-distance
    /// suggestion) and repeated fields.
    fn parse_beam_field(&mut self, fields: &mut BeamFields) -> Result<(), ParseError> {
        let field_tok = self.eat_ident()?;
        let name = field_tok.value.as_str();

        if !BEAM_FIELDS.contains(&name) {
            let help = suggest_field(name).map(|f| format!("did you mean `{f}`?"));
            return Err(ParseError {
                message: format!("unknown field `{name}`"),
                span: field_tok.span,
                help,
            });
        }

        if !fields.seen.insert(name.to_string()) {
            return Err(ParseError {
                message: format!("duplicate field `{name}`"),
                span: field_tok.span,
                help: None,
            });
        }

        match name {
            "description" => fields.description = Some(self.eat_template()?),
            "needs" => fields.needs = self.parse_needs_list()?,
            "inputs" => fields.inputs = self.parse_template_list()?,
            "outputs" => fields.outputs = self.parse_template_list()?,
            "run" => fields.run = self.parse_run_value()?,
            "env" => fields.env = self.parse_env_block()?,
            "cwd" => fields.cwd = Some(self.eat_template()?),
            "executor" => fields.executor = Some(self.parse_executor_decl()?),
            "allow_failure" => fields.allow_failure = self.parse_bool_value()?,
            other => unreachable!("field `{other}` accepted by BEAM_FIELDS but not dispatched"),
        }
        Ok(())
    }

    /// `beam name(params) { field field ... }`.
    fn parse_beam(&mut self) -> Result<BeamDecl, ParseError> {
        let beam_kw = self.expect(TokenKind::KwBeam, "`beam`")?;
        let name = self.eat_ident()?;

        let mut params = Vec::new();
        if self.check(&TokenKind::LParen) {
            self.advance();
            if !self.check(&TokenKind::RParen) {
                loop {
                    params.push(self.eat_ident()?);
                    if self.check(&TokenKind::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            self.expect(TokenKind::RParen, "`)`")?;
        }

        self.expect(TokenKind::LBrace, "`{`")?;
        let mut fields = BeamFields::default();
        while !self.check(&TokenKind::RBrace) {
            self.parse_beam_field(&mut fields)?;
        }
        let rbrace = self.expect(TokenKind::RBrace, "`}`")?;

        Ok(BeamDecl {
            name,
            params,
            description: fields.description,
            needs: fields.needs,
            inputs: fields.inputs,
            outputs: fields.outputs,
            run: fields.run,
            env: fields.env,
            cwd: fields.cwd,
            executor: fields.executor,
            allow_failure: fields.allow_failure,
            span: Span::new(beam_kw.span.start, rbrace.span.end),
        })
    }

    /// `import "path" as alias`.
    fn parse_import(&mut self) -> Result<Import, ParseError> {
        self.expect(TokenKind::KwImport, "`import`")?;
        let path = self.eat_str()?;
        self.expect(TokenKind::KwAs, "`as`")?;
        let alias = self.eat_ident()?;
        Ok(Import { path, alias })
    }

    /// `let name = expr`.
    fn parse_let(&mut self) -> Result<LetBinding, ParseError> {
        self.expect(TokenKind::KwLet, "`let`")?;
        let name = self.eat_ident()?;
        self.expect(TokenKind::Eq, "`=`")?;
        let value = self.parse_expr()?;
        Ok(LetBinding { name, value })
    }

    /// Parses a whole file: `version`, `import`s, `let`s, `default`, and
    /// `beam` declarations, in any order, followed by end of input.
    /// Detects duplicate beam names once every beam has been parsed.
    fn parse_file(&mut self) -> Result<File, ParseError> {
        let mut version = None;
        let mut imports = Vec::new();
        let mut lets = Vec::new();
        let mut default = None;
        let mut beams = Vec::new();

        while !self.is_eof() {
            match self.peek().kind {
                TokenKind::KwVersion => {
                    self.advance();
                    version = Some(self.eat_str()?);
                }
                TokenKind::KwImport => imports.push(self.parse_import()?),
                TokenKind::KwLet => lets.push(self.parse_let()?),
                TokenKind::KwDefault => {
                    self.advance();
                    default = Some(self.eat_ident()?);
                }
                TokenKind::KwBeam => beams.push(self.parse_beam()?),
                _ => {
                    return Err(self.unexpected("`version`, `import`, `let`, `default`, or `beam`"));
                }
            }
        }

        let mut seen_beam_names = HashSet::new();
        for beam in &beams {
            if !seen_beam_names.insert(beam.name.value.clone()) {
                return Err(ParseError {
                    message: format!("duplicate beam `{}`", beam.name.value),
                    span: beam.name.span,
                    help: None,
                });
            }
        }

        Ok(File {
            version,
            imports,
            lets,
            default,
            beams,
        })
    }
}

/// A short, human-readable description of a token kind for error messages.
fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Ident(name) => format!("identifier `{name}`"),
        TokenKind::Str(_) => "a string literal".to_string(),
        TokenKind::KwVersion => "`version`".to_string(),
        TokenKind::KwImport => "`import`".to_string(),
        TokenKind::KwAs => "`as`".to_string(),
        TokenKind::KwLet => "`let`".to_string(),
        TokenKind::KwDefault => "`default`".to_string(),
        TokenKind::KwBeam => "`beam`".to_string(),
        TokenKind::KwIf => "`if`".to_string(),
        TokenKind::KwThen => "`then`".to_string(),
        TokenKind::KwElse => "`else`".to_string(),
        TokenKind::KwTrue => "`true`".to_string(),
        TokenKind::KwFalse => "`false`".to_string(),
        TokenKind::LBrace => "`{`".to_string(),
        TokenKind::RBrace => "`}`".to_string(),
        TokenKind::LBracket => "`[`".to_string(),
        TokenKind::RBracket => "`]`".to_string(),
        TokenKind::LParen => "`(`".to_string(),
        TokenKind::RParen => "`)`".to_string(),
        TokenKind::Comma => "`,`".to_string(),
        TokenKind::Colon => "`:`".to_string(),
        TokenKind::Eq => "`=`".to_string(),
        TokenKind::EqEq => "`==`".to_string(),
        TokenKind::NotEq => "`!=`".to_string(),
        TokenKind::AndAnd => "`&&`".to_string(),
        TokenKind::OrOr => "`||`".to_string(),
        TokenKind::Plus => "`+`".to_string(),
        TokenKind::Newline => "a newline".to_string(),
        TokenKind::Eof => "end of input".to_string(),
    }
}

/// Suggests the closest known beam field name to `name`, if any is within
/// Levenshtein distance 2 (picking the closest on ties by array order).
fn suggest_field(name: &str) -> Option<&'static str> {
    BEAM_FIELDS
        .iter()
        .map(|&field| (field, levenshtein(name, field)))
        .filter(|&(_, distance)| distance <= 2)
        .min_by_key(|&(_, distance)| distance)
        .map(|(field, _)| field)
}

/// Classic dynamic-programming Levenshtein edit distance between two
/// strings, operating on `char`s (not bytes) so it stays correct for
/// non-ASCII field-name typos.
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

#[cfg(test)]
mod tests {
    use crate::parse;

    #[test]
    fn parses_full_beamfile() {
        let src = r#"
version "1"
import "api/Beamfile" as api
let profile = env("PROFILE", "debug")
default build

beam build {
  description "Compile"
  needs [api:build, codegen]
  inputs ["src/**/*.rs"]
  run "cargo build"
}

beam deploy(target) {
  needs [build]
  allow_failure true
  run ["echo one", "echo two"]
}
"#;
        let file = parse(src).unwrap();
        assert_eq!(file.beams.len(), 2);
        assert_eq!(file.default.as_ref().unwrap().value, "build");
        let build = &file.beams[0];
        assert_eq!(build.needs[0].value.namespace.as_deref(), Some("api"));
        let deploy = &file.beams[1];
        assert_eq!(deploy.params[0].value, "target");
        assert!(deploy.allow_failure);
        assert_eq!(deploy.run.len(), 2);
    }

    #[test]
    fn rejects_unknown_beam_field_with_help() {
        let err = parse("beam x { descriptoin \"typo\" }").unwrap_err();
        assert!(err.message.contains("unknown field"));
        assert_eq!(err.help.as_deref(), Some("did you mean `description`?"));
    }

    #[test]
    fn rejects_duplicate_beam_names() {
        let err = parse("beam x { run \"a\" }\nbeam x { run \"b\" }").unwrap_err();
        assert!(err.message.contains("duplicate beam"));
    }

    #[test]
    fn rejects_duplicate_field_in_same_beam() {
        let err = parse("beam x { run \"a\" run \"b\" }").unwrap_err();
        assert!(err.message.contains("duplicate field"));
    }

    #[test]
    fn needs_list_accepts_trailing_comma_and_newlines() {
        let file = parse("beam x {\n  needs [\n    a,\n    b,\n  ]\n}").unwrap();
        let needs = &file.beams[0].needs;
        assert_eq!(needs.len(), 2);
        assert_eq!(needs[0].value.name, "a");
        assert_eq!(needs[1].value.name, "b");
    }

    #[test]
    fn parses_let_with_equality_expression() {
        let file = parse(r#"let release = profile == "release""#).unwrap();
        assert_eq!(file.lets.len(), 1);
        assert_eq!(file.lets[0].name.value, "release");
    }

    #[test]
    fn parses_env_and_executor_blocks() {
        let file = parse(
            r#"beam deploy(target) {
  executor docker { image "deployer:latest" }
  env { DEPLOY_TARGET = target }
  run "./scripts/deploy.sh {target}"
}"#,
        )
        .unwrap();
        let beam = &file.beams[0];
        let executor = beam.executor.as_ref().unwrap();
        assert_eq!(executor.name.value, "docker");
        assert_eq!(executor.options[0].0.value, "image");
        assert_eq!(
            executor.options[0].1.parts,
            vec![crate::TemplatePart::Literal("deployer:latest".to_string())]
        );
        assert_eq!(beam.env[0].0.value, "DEPLOY_TARGET");
        // `target` is a bare identifier (no quotes), so it becomes a
        // single-part template wrapping a variable reference, equivalent
        // to writing `"{target}"`.
        assert!(matches!(
            &beam.env[0].1.parts[..],
            [crate::TemplatePart::Expr(crate::Expr::Var(v))] if v.value == "target"
        ));
    }

    #[test]
    fn reports_unexpected_token_span_not_enclosing_construct() {
        let err = parse("beam x { needs [a, }").unwrap_err();
        // The span must point at the offending `}`, not at `needs` or `beam`.
        assert_eq!(err.span, crate::Span::new(19, 20));
    }
}
