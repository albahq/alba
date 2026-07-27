//! [`StringTemplate`], [`TemplatePart`], and string interpolation parsing.
//!
//! ## Source-position tracking
//!
//! The lexer's [`crate::token::TokenKind::Str`] holds the string's content
//! with `\"`/`\\`/`\n`/`\t` escapes already decoded, but interpolation
//! braces left as raw text. That decoded content is *not* used here: once
//! escapes are resolved, the content's byte offsets no longer line up
//! one-to-one with the original source (each decoded `\n` is 1 byte where
//! the source had 2), and every span this module produces must point into
//! the *original* source so Task 5 can put a caret under it.
//!
//! Instead, [`parse_template_at`] re-scans `full_source[span]` directly
//! (the literal text as written, quotes included) and decodes escapes
//! itself while walking `full_source`'s own byte offsets, so every
//! position it computes is automatically a real source offset — no
//! separate offset-mapping table is needed. This does mean re-implementing
//! the same four-entry escape table the lexer already has; that
//! duplication is small and deliberate; see [`parse_template_at`]'s doc
//! comment for why it is safe to make the decode step of that
//! re-implementation infallible.
//!
//! ## Interpolation body parsing
//!
//! `{expr}` is resolved by [`scan_interpolation`], which re-lexes the
//! remaining source one token at a time (via the ordinary
//! [`crate::lexer::Lexer`], bounded to this string literal's own content so
//! it can never wander into unrelated later source) and stops at the first
//! `RBrace`. That token is unambiguously the interpolation's closing brace:
//! the expression grammar has no other use for `{`/`}`, and any brace
//! *inside* a nested string literal (`{a + "{b}"}`) is swallowed whole by
//! that string's own `Str` token, since the lexer never tokenizes inside a
//! string — so it never surfaces as a stray `LBrace`/`RBrace` here. This is
//! also what makes nested interpolation work for free: the nested string's
//! own `{b}` is resolved later, recursively, when that `Str` token is
//! itself turned into a `StringTemplate`.
//!
//! `{}` (nothing between the braces) is rejected as an "empty
//! interpolation" error. A `{` with no matching `}` before the end of the
//! string is rejected as an "unclosed interpolation" error, spanning just
//! the offending `{`.

use crate::expr::Expr;
use crate::lexer::Lexer;
use crate::parser::{ParseError, Parser};
use crate::token::{Span, Token, TokenKind};

/// A string literal parsed into alternating literal text and interpolated
/// expressions, e.g. `"target/{profile}/app"` becomes
/// `[Literal("target/"), Expr(Var(profile)), Literal("/app")]`.
#[derive(Debug, Clone, PartialEq)]
pub struct StringTemplate {
    pub parts: Vec<TemplatePart>,
    pub span: Span,
}

/// One piece of a [`StringTemplate`]: either literal text or a `{expr}`
/// interpolation.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplatePart {
    Literal(String),
    Expr(Expr),
}

/// Parses a single quoted string literal (e.g. `"target/{profile}/app"`,
/// quotes included) into a [`StringTemplate`], treating `source` as though
/// it were the whole world: spans in the result are 0-based byte offsets
/// into `source` itself. Use [`parse_template_at`] when the literal is
/// embedded in a larger file and spans must land in that original source.
pub fn parse_template(source: &str) -> Result<StringTemplate, ParseError> {
    parse_template_at(source, Span::new(0, source.len()))
}

/// Parses the string literal at `span` (including its surrounding quotes)
/// within `full_source` into a [`StringTemplate`], with every span in the
/// result a byte offset into `full_source`.
///
/// `span` is always the span of a string literal the lexer already
/// tokenized successfully once — either the original top-level token, or
/// (recursively) a nested string literal found while re-lexing an
/// interpolation body below. Either way, the lexer already validated its
/// escapes and its closing quote, so decoding them again here can never
/// fail; the `unreachable!` in the escape match documents that invariant
/// rather than guarding against it.
pub(crate) fn parse_template_at(
    full_source: &str,
    span: Span,
) -> Result<StringTemplate, ParseError> {
    let raw = &full_source[span.start..span.end];
    let quote_len = raw
        .chars()
        .next()
        .expect("a string literal's span covers at least its quotes")
        .len_utf8();
    let content_end = span.end - quote_len;

    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut pos = span.start + quote_len;

    while pos < content_end {
        let c = full_source[pos..]
            .chars()
            .next()
            .expect("pos is a valid char boundary within full_source");
        match c {
            '\\' => {
                let escaped = full_source[pos + 1..]
                    .chars()
                    .next()
                    .expect("a trailing backslash would have failed lexing already");
                let decoded = match escaped {
                    '"' => '"',
                    '\\' => '\\',
                    'n' => '\n',
                    't' => '\t',
                    other => unreachable!("escape '\\{other}' would have failed lexing already"),
                };
                literal.push(decoded);
                pos += 1 + escaped.len_utf8();
            }
            '{' if full_source[pos + 1..].starts_with('{') => {
                literal.push('{');
                pos += 2;
            }
            '}' if full_source[pos + 1..].starts_with('}') => {
                literal.push('}');
                pos += 2;
            }
            '{' => {
                if !literal.is_empty() {
                    parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                }
                let (expr, after) = scan_interpolation(full_source, pos, content_end)?;
                parts.push(TemplatePart::Expr(expr));
                pos = after;
            }
            other => {
                literal.push(other);
                pos += other.len_utf8();
            }
        }
    }

    if !literal.is_empty() || parts.is_empty() {
        parts.push(TemplatePart::Literal(literal));
    }

    Ok(StringTemplate { parts, span })
}

/// Parses the interpolation expression opened by the `{` at `brace_pos` (an
/// absolute byte offset into `full_source`), never reading past
/// `content_end` (the enclosing string literal's own closing quote).
/// Returns the parsed expression and the absolute offset just past its
/// closing `}`.
fn scan_interpolation(
    full_source: &str,
    brace_pos: usize,
    content_end: usize,
) -> Result<(Expr, usize), ParseError> {
    let body_start = brace_pos + 1;
    let mut lexer = Lexer::new(&full_source[body_start..content_end]);
    let mut tokens = Vec::new();
    let mut close_end = None;

    loop {
        match lexer.next() {
            Some(Ok(tok)) if tok.kind == TokenKind::RBrace => {
                close_end = Some(body_start + tok.span.end);
                break;
            }
            Some(Ok(tok)) if tok.kind == TokenKind::Eof => break,
            // Newline tokens carry no grammatical meaning (see
            // parser.rs); drop them and keep scanning.
            Some(Ok(tok)) if tok.kind == TokenKind::Newline => {}
            Some(Ok(tok)) => tokens.push(shift(tok, body_start)),
            Some(Err(_)) | None => break,
        }
    }

    let Some(close_end) = close_end else {
        return Err(ParseError {
            message: "unclosed interpolation".to_string(),
            span: Span::new(brace_pos, brace_pos + 1),
            help: None,
        });
    };

    if tokens.is_empty() {
        return Err(ParseError {
            message: "empty interpolation".to_string(),
            span: Span::new(brace_pos, close_end),
            help: None,
        });
    }

    // A synthetic Eof so the ordinary Parser machinery can confirm the
    // body is exactly one expression with nothing trailing before `}`.
    let eof_span = Span::new(close_end - 1, close_end - 1);
    tokens.push(Token::new(TokenKind::Eof, eof_span));

    let mut parser = Parser::new(tokens, full_source);
    let expr = parser.parse_expr()?;
    if !parser.is_eof() {
        return Err(parser.unexpected("`}`"));
    }
    Ok((expr, close_end))
}

/// Shifts a token lexed from a bounded sub-slice of `full_source` so its
/// span points at its real, absolute position in `full_source`.
fn shift(token: Token, offset: usize) -> Token {
    Token::new(
        token.kind,
        Span::new(token.span.start + offset, token.span.end + offset),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interpolation() {
        let t = parse_template(r#""target/{profile}/app""#).unwrap();
        assert!(matches!(&t.parts[0], TemplatePart::Literal(s) if s == "target/"));
        assert!(matches!(&t.parts[1], TemplatePart::Expr(Expr::Var(v)) if v.value == "profile"));
    }

    #[test]
    fn double_brace_escapes() {
        let t = parse_template(r#""a {{literal}} b""#).unwrap();
        assert!(matches!(&t.parts[0], TemplatePart::Literal(s) if s == "a {literal} b"));
    }

    #[test]
    fn rejects_unclosed_interpolation() {
        let err = crate::parse(r#"beam x { run "echo {oops" }"#).unwrap_err();
        assert!(err.message.contains("unclosed interpolation"));
    }
}
