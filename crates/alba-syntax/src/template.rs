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
//! `{expr}` is resolved by [`scan_interpolation`], which lexes the rest of
//! the string literal's content (via the ordinary [`crate::lexer::Lexer`],
//! bounded to that content so it can never wander into unrelated later
//! source), parses one expression from those tokens, and then requires a
//! `}`. The parser decides where the expression ends, so a `match` body's
//! own braces are consumed by the `match` itself and the interpolation
//! closes on the `}` that follows. Any brace *inside* a nested string
//! literal (`{a + "{b}"}`) is swallowed whole by that string's own `Str`
//! token, since the lexer never tokenizes inside a string; the nested
//! string's own `{b}` is resolved later, recursively, when that `Str` token
//! is itself turned into a `StringTemplate`.
//!
//! The text after the closing `}` is literal text, not source code: it may
//! hold a lone quote, a backslash, or a character the grammar has no use
//! for. So a lex error there must not fail the string. Lexing stops at the
//! first error, which is kept aside and replaced by an end-of-input
//! sentinel: only if the parser runs into that sentinel (the expression
//! needed more tokens than lexed cleanly) is the lex error reported, as
//! itself and at its own precise span. A lex error past the closing `}` is
//! never reached and so never reported.
//!
//! `{}` (nothing between the braces) is rejected as an "empty
//! interpolation" error. A `{` whose expression runs out of string before a
//! `}` is rejected as an "unclosed interpolation" error, spanning just the
//! offending `{`. A complete expression followed by anything other than
//! `}` is rejected as "expected `}` to close the interpolation", pointing
//! at that token.

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

/// Parses the string literal at `span` as a `match` pattern: its decoded
/// literal text, with `{{`/`}}` standing for literal braces as everywhere
/// else. A pattern must be known at parse time, so an interpolation in it
/// is an error at its opening `{`. The same `span` contract as
/// [`parse_template_at`] applies.
pub(crate) fn parse_pattern_at(full_source: &str, span: Span) -> Result<String, ParseError> {
    if let Some(brace) = first_interpolation_brace(full_source, span) {
        return Err(ParseError {
            message: "a `match` pattern cannot interpolate".to_string(),
            span: Span::new(brace, brace + 1),
            help: Some(
                "patterns must be known when the Beamfile loads; write a literal brace as `{{` \
                 or `}}`"
                    .to_string(),
            ),
        });
    }
    let template = parse_template_at(full_source, span)?;
    Ok(template
        .parts
        .into_iter()
        .map(|part| match part {
            TemplatePart::Literal(text) => text,
            TemplatePart::Expr(_) => unreachable!("an interpolation was rejected above"),
        })
        .collect())
}

/// The absolute offset of the first `{` opening an interpolation in the
/// string literal at `span`, skipping escapes and `{{`/`}}`, exactly as
/// [`parse_template_at`] walks it.
fn first_interpolation_brace(full_source: &str, span: Span) -> Option<usize> {
    let raw = &full_source[span.start..span.end];
    let quote_len = raw.chars().next()?.len_utf8();
    let content = &full_source[span.start + quote_len..span.end - quote_len];
    let mut chars = content.char_indices().peekable();
    while let Some((offset, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '{' | '}' if chars.peek().map(|&(_, next)| next) == Some(c) => {
                chars.next();
            }
            '{' => return Some(span.start + quote_len + offset),
            _ => {}
        }
    }
    None
}

/// Parses the interpolation expression opened by the `{` at `brace_pos` (an
/// absolute byte offset into `full_source`), never reading past
/// `content_end` (the enclosing string literal's own closing quote).
/// Returns the parsed expression and the absolute offset just past its
/// closing `}`. See the module doc comment for how the end is found and
/// why a lex error is only reported once the parser reaches it.
fn scan_interpolation(
    full_source: &str,
    brace_pos: usize,
    content_end: usize,
) -> Result<(Expr, usize), ParseError> {
    let body_start = brace_pos + 1;
    let mut tokens = Vec::new();
    let mut lex_error = None;
    for result in Lexer::new(&full_source[body_start..content_end]) {
        match result {
            // Newline tokens carry no grammatical meaning (see
            // parser.rs); drop them.
            Ok(tok) if tok.kind == TokenKind::Newline => {}
            Ok(tok) if tok.kind == TokenKind::Eof => break,
            Ok(tok) => tokens.push(shift(tok, body_start)),
            Err(error) => {
                lex_error = Some(error);
                break;
            }
        }
    }

    // The sentinel ending the token stream: zero-width, so no real token
    // can share its span, at the lex error if lexing stopped on one, else
    // at the end of the string's content.
    let sentinel = match &lex_error {
        Some(error) => Span::new(body_start + error.span.start, body_start + error.span.start),
        None => Span::new(content_end, content_end),
    };
    let cut_short = |err: ParseError| -> ParseError {
        if err.span != sentinel {
            return err;
        }
        match &lex_error {
            Some(error) => {
                let span = shift_span(error.span, body_start);
                ParseError {
                    message: error.message.clone(),
                    help: escaped_quote_help(full_source, span),
                    span,
                }
            }
            None => ParseError {
                message: "unclosed interpolation".to_string(),
                span: Span::new(brace_pos, brace_pos + 1),
                help: None,
            },
        }
    };

    if let Some(close) = tokens.first().filter(|tok| tok.kind == TokenKind::RBrace) {
        return Err(ParseError {
            message: "empty interpolation".to_string(),
            span: Span::new(brace_pos, close.span.end),
            help: None,
        });
    }
    tokens.push(Token::new(TokenKind::Eof, sentinel));

    let mut parser = Parser::new(tokens, full_source);
    let expr = parser.parse_expr().map_err(cut_short)?;
    let close = parser
        .expect(TokenKind::RBrace, "`}` to close the interpolation")
        .map_err(cut_short)?;
    Ok((expr, close.span.end))
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
