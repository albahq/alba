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
//! the *original* source so a diagnostic can put a caret under it.
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
//! the offending `{` — but only when the body genuinely never closes. A
//! real lex error inside the body (an unexpected character, an
//! unterminated nested string, ...) is propagated as itself, at its own
//! precise span, rather than swallowed and misreported as "unclosed
//! interpolation": the interpolation *is* syntactically closed in that
//! case, and "unclosed" would send a reader to the wrong byte.

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
///
/// This is a real public entry point, not just a test helper, so it holds
/// itself to a public API's input contract: `source` is lexed first (the
/// same way `parser::parse` lexes a whole file), and anything other than
/// exactly one string literal followed by end of input — an empty string,
/// unquoted text, a bad escape, an unterminated literal, trailing
/// content, and so on — is rejected with a `ParseError` rather than
/// panicking or silently misparsing. That validation is what lets
/// [`parse_template_at`] below assume its `span` always names a string
/// the lexer already accepted.
pub fn parse_template(source: &str) -> Result<StringTemplate, ParseError> {
    let tokens = crate::parser::tokenize(source)?;
    match &tokens[..] {
        [
            Token {
                kind: TokenKind::Str(_),
                span,
            },
            Token {
                kind: TokenKind::Eof,
                ..
            },
        ] => parse_template_at(source, *span),
        _ => Err(ParseError {
            message: "expected a single string literal".to_string(),
            span: Span::new(0, source.len()),
            help: None,
        }),
    }
}

/// Parses the string literal at `span` (including its surrounding quotes)
/// within `full_source` into a [`StringTemplate`], with every span in the
/// result a byte offset into `full_source`.
///
/// `span` must be the span of a string literal the lexer has already
/// tokenized successfully: either the original top-level token, a literal
/// [`parse_template`] validated by lexing `full_source` itself, or
/// (recursively) a nested string literal found while re-lexing an
/// interpolation body below. In every case the lexer already validated its
/// escapes and its closing quote, so decoding them again here can never
/// fail; the `unreachable!` in the escape match documents that invariant
/// rather than guarding against it. Callers that cannot guarantee this —
/// i.e. anyone outside this crate — should go through [`parse_template`]
/// instead, which lexes first.
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
            // A real lex error (e.g. an unexpected character, or an
            // unterminated nested string) is the actual fault here, and a
            // far more precise diagnosis than "unclosed interpolation" —
            // propagate it, shifted onto `full_source`, instead of
            // discarding it and falling through to that generic message.
            Some(Err(e)) => {
                let span = shift_span(e.span, body_start);
                return Err(ParseError {
                    message: e.message,
                    help: escaped_quote_help(full_source, span),
                    span,
                });
            }
            // The lexer only ever yields `None` after it has already
            // yielded `Eof` once (see `Lexer`'s own doc comment), and the
            // arm above already breaks on that `Eof` — so this is not
            // reachable in practice. Treated the same as running out of
            // input, for defense in depth rather than `unreachable!()`.
            None => break,
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

    // A synthetic Eof — spanning the closing `}` itself — so the ordinary
    // Parser machinery can confirm the body is exactly one expression with
    // nothing trailing before `}`. Should an error land exactly on this
    // sentinel (e.g. `{b +}`, where an operand is missing right before the
    // brace), `describe`'s generic "end of input" phrasing would be a lie —
    // the `}` is right there — so `fixup_eof_message` corrects it.
    let eof_span = Span::new(close_end - 1, close_end);
    tokens.push(Token::new(TokenKind::Eof, eof_span));

    let mut parser = Parser::new(tokens, full_source);
    let expr = parser
        .parse_expr()
        .map_err(|err| fixup_eof_message(err, eof_span))?;
    if !parser.is_eof() {
        return Err(parser.unexpected("`}`"));
    }
    Ok((expr, close_end))
}

/// Help text for the one lex failure inside an interpolation body that a
/// reader is likely to hit by writing perfectly reasonable-looking code:
/// escaping a quote, as in `run "echo {env(\"VAR\")}"`. Escapes are
/// decoded in a string's literal parts, but an interpolation body is
/// re-lexed straight from the source, so the backslash reaches the lexer
/// as itself and is rejected as a stray character. Single quotes need no
/// escaping and work, which is what this points at.
fn escaped_quote_help(full_source: &str, span: Span) -> Option<String> {
    full_source
        .get(span.start..span.end)?
        .starts_with('\\')
        .then(|| "use single quotes inside an interpolation, e.g. `{env('VAR')}`".to_string())
}

/// Rewrites an error's "found end of input" phrasing to name the
/// interpolation's closing `}` instead, when the error's span is exactly
/// `eof_span` (the synthetic sentinel `scan_interpolation` feeds the
/// sub-parser). Every other error — including the `!parser.is_eof()`
/// check above, which only ever fires on a real trailing token — is
/// returned unchanged.
fn fixup_eof_message(mut err: ParseError, eof_span: Span) -> ParseError {
    if err.span == eof_span {
        err.message = err
            .message
            .replacen(crate::parser::EOF_DESCRIPTION, "`}`", 1);
    }
    err
}

/// Shifts a token lexed from a bounded sub-slice of `full_source` so its
/// span points at its real, absolute position in `full_source`.
fn shift(token: Token, offset: usize) -> Token {
    Token::new(token.kind, shift_span(token.span, offset))
}

/// Shifts a span computed against a bounded sub-slice of `full_source` (as
/// `scan_interpolation`'s sub-lexer produces, both for tokens and for its
/// own lex errors) onto that slice's real, absolute position in
/// `full_source`.
fn shift_span(span: Span, offset: usize) -> Span {
    Span::new(span.start + offset, span.end + offset)
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

    #[test]
    fn unclosed_interpolation_span_points_at_the_open_brace() {
        let err = parse_template(r#""a{oops""#).unwrap_err();
        assert_eq!(err.span, Span::new(2, 3));
        assert!(err.message.contains("unclosed interpolation"));
    }

    #[test]
    fn empty_interpolation_span_covers_both_braces() {
        let err = parse_template(r#""a{}b""#).unwrap_err();
        assert_eq!(err.span, Span::new(2, 4));
        assert!(err.message.contains("empty interpolation"));
    }

    /// A syntactically closed interpolation whose body contains a real lex
    /// error (an unexpected character) must report that error, at the
    /// character's own span — not get misdiagnosed as "unclosed
    /// interpolation" just because the sub-lexer that's hunting for the
    /// closing `}` happened to fail before reaching one.
    #[test]
    fn interpolation_reports_the_real_lex_error_not_unclosed() {
        let err = parse_template(r#""a{b @ c}""#).unwrap_err();
        assert_eq!(err.span, Span::new(5, 6));
        assert!(err.message.contains('@'));
        assert!(!err.message.contains("unclosed"));
    }

    #[test]
    fn missing_operand_before_close_brace_names_the_brace_not_end_of_input() {
        let err = parse_template(r#""a{b +}""#).unwrap_err();
        assert_eq!(err.span, Span::new(6, 7));
        assert!(err.message.contains('}'));
        assert!(!err.message.contains("end of input"));
    }

    /// An interpolation body is re-lexed from the raw source, so a `\"`
    /// escape that works perfectly well in the literal parts of a string
    /// is a stray backslash inside `{...}`. The message says so, and the
    /// help names the form that does work.
    #[test]
    fn an_escaped_quote_inside_an_interpolation_suggests_single_quotes() {
        let err = parse_template(r#""echo {env(\"VAR\")}""#).unwrap_err();

        assert_eq!(err.span, Span::new(11, 12));
        assert!(err.message.contains('\\'));
        assert_eq!(
            err.help.as_deref(),
            Some("use single quotes inside an interpolation, e.g. `{env('VAR')}`")
        );
    }

    #[test]
    fn rejects_empty_input_instead_of_panicking() {
        assert!(parse_template("").is_err());
    }

    #[test]
    fn rejects_invalid_escape_instead_of_panicking() {
        let err = parse_template(r#""a\qb""#).unwrap_err();
        assert!(err.message.contains("unknown escape"));
    }

    #[test]
    fn rejects_unquoted_text_instead_of_silently_truncating() {
        assert!(parse_template("hello").is_err());
    }

    #[test]
    fn rejects_unterminated_string_instead_of_silently_truncating() {
        let err = parse_template(r#""ab\"#).unwrap_err();
        assert!(err.message.contains("unterminated string"));
    }

    #[test]
    fn rejects_trailing_content_after_the_string_literal() {
        assert!(parse_template(r#""a" "b""#).is_err());
    }
}
