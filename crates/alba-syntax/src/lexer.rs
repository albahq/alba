//! Hand-written scanner turning Beamfile source text into a stream of tokens.

use crate::token::{Span, Token, TokenKind};
use std::iter::Peekable;
use std::str::CharIndices;

/// An error produced while scanning source text into tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    pub message: String,
    pub span: Span,
}

impl LexError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }
}

/// Scans Beamfile source text into a stream of [`Token`]s.
///
/// `Lexer` implements `Iterator<Item = Result<Token, LexError>>`: each call
/// to `next` yields either a token or a scanning error at a precise span.
///
/// Newline handling: a `Newline` token is emitted only strictly between two
/// other tokens. Leading and trailing newline trivia (including blank lines
/// and comments, which never produce tokens of their own) are dropped
/// entirely, and any run of consecutive newlines collapses into a single
/// `Newline` token. Exactly one `Eof` token is yielded at the end of input,
/// never preceded by a `Newline`; after that, the iterator yields `None`.
pub struct Lexer<'src> {
    source: &'src str,
    chars: Peekable<CharIndices<'src>>,
    /// True once at least one non-trivia token has been produced.
    after_first_token: bool,
    /// True if a newline has been seen since the last produced token.
    pending_newline: bool,
    /// Byte offset of the first newline in the current pending run.
    pending_newline_start: usize,
    /// True once `Eof` has been produced; further calls return `None`.
    eof_emitted: bool,
}

impl<'src> Lexer<'src> {
    /// Creates a lexer over `source`.
    pub fn new(source: &'src str) -> Self {
        Self {
            source,
            chars: source.char_indices().peekable(),
            after_first_token: false,
            pending_newline: false,
            pending_newline_start: 0,
            eof_emitted: false,
        }
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|&(_, c)| c)
    }

    /// Skips whitespace and comments, tracking whether a newline was seen.
    /// Leaves the iterator positioned at the start of the next real token,
    /// or at the end of input.
    fn skip_trivia(&mut self) {
        loop {
            match self.peek_char() {
                Some(' ') | Some('\t') | Some('\r') => {
                    self.chars.next();
                }
                Some('\n') => {
                    let (idx, _) = self.chars.next().unwrap();
                    if !self.pending_newline {
                        self.pending_newline = true;
                        self.pending_newline_start = idx;
                    }
                }
                Some('#') => {
                    while let Some(c) = self.peek_char() {
                        if c == '\n' {
                            break;
                        }
                        self.chars.next();
                    }
                }
                _ => break,
            }
        }
    }

    /// Hyphens are identifier characters after the first one (git hook names
    /// are spelled `pre-commit`); the DSL has no subtraction, so nothing
    /// else could claim them.
    fn scan_ident(&mut self, start: usize) -> Token {
        let mut end = start;
        while let Some(&(idx, c)) = self.chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                end = idx + c.len_utf8();
                self.chars.next();
            } else {
                break;
            }
        }
        let text = &self.source[start..end];
        let kind = keyword_kind(text).unwrap_or_else(|| TokenKind::Ident(text.to_string()));
        Token::new(kind, Span::new(start, end))
    }

    fn scan_string(&mut self, start: usize, quote: char) -> Result<Token, LexError> {
        self.chars.next(); // consume the opening quote
        let mut content = String::new();
        loop {
            match self.chars.next() {
                None => {
                    return Err(LexError::new(
                        "unterminated string literal",
                        Span::new(start, self.source.len()),
                    ));
                }
                Some((idx, c)) if c == quote => {
                    let end = idx + c.len_utf8();
                    return Ok(Token::new(TokenKind::Str(content), Span::new(start, end)));
                }
                Some((_, '\\')) => match self.chars.next() {
                    None => {
                        return Err(LexError::new(
                            "unterminated string literal",
                            Span::new(start, self.source.len()),
                        ));
                    }
                    Some((idx, escaped)) => {
                        let decoded = match escaped {
                            '"' => '"',
                            '\\' => '\\',
                            'n' => '\n',
                            't' => '\t',
                            other => {
                                return Err(LexError::new(
                                    format!("unknown escape sequence '\\{other}'"),
                                    Span::new(idx - 1, idx + other.len_utf8()),
                                ));
                            }
                        };
                        content.push(decoded);
                    }
                },
                Some((_, c)) => content.push(c),
            }
        }
    }

    /// Scans a token that starts with `first`, given `first` has already
    /// been peeked but not consumed. Consumes one or two characters and
    /// returns the resulting punctuation/operator token, or an error for an
    /// unrecognized character.
    fn scan_punct(&mut self, start: usize, first: char) -> Result<Token, LexError> {
        self.chars.next(); // consume `first`
        let single = |kind: TokenKind| Ok(Token::new(kind, Span::new(start, start + 1)));
        match first {
            '{' => single(TokenKind::LBrace),
            '}' => single(TokenKind::RBrace),
            '[' => single(TokenKind::LBracket),
            ']' => single(TokenKind::RBracket),
            '(' => single(TokenKind::LParen),
            ')' => single(TokenKind::RParen),
            ',' => single(TokenKind::Comma),
            ':' => single(TokenKind::Colon),
            '+' => single(TokenKind::Plus),
            '.' => single(TokenKind::Dot),
            '=' => {
                if self.peek_char() == Some('=') {
                    self.chars.next();
                    Ok(Token::new(TokenKind::EqEq, Span::new(start, start + 2)))
                } else {
                    single(TokenKind::Eq)
                }
            }
            '!' => {
                if self.peek_char() == Some('=') {
                    self.chars.next();
                    Ok(Token::new(TokenKind::NotEq, Span::new(start, start + 2)))
                } else {
                    Err(LexError::new(
                        "unexpected character '!'",
                        Span::new(start, start + 1),
                    ))
                }
            }
            '&' => {
                if self.peek_char() == Some('&') {
                    self.chars.next();
                    Ok(Token::new(TokenKind::AndAnd, Span::new(start, start + 2)))
                } else {
                    Err(LexError::new(
                        "unexpected character '&'",
                        Span::new(start, start + 1),
                    ))
                }
            }
            '|' => {
                if self.peek_char() == Some('|') {
                    self.chars.next();
                    Ok(Token::new(TokenKind::OrOr, Span::new(start, start + 2)))
                } else {
                    Err(LexError::new(
                        "unexpected character '|'",
                        Span::new(start, start + 1),
                    ))
                }
            }
            other => Err(LexError::new(
                format!("unexpected character '{other}'"),
                Span::new(start, start + other.len_utf8()),
            )),
        }
    }
}

/// Resolves an identifier's text to a keyword token kind, if it is one.
fn keyword_kind(text: &str) -> Option<TokenKind> {
    Some(match text {
        "version" => TokenKind::KwVersion,
        "import" => TokenKind::KwImport,
        "as" => TokenKind::KwAs,
        "let" => TokenKind::KwLet,
        "default" => TokenKind::KwDefault,
        "beam" => TokenKind::KwBeam,
        "hook" => TokenKind::KwHook,
        "if" => TokenKind::KwIf,
        "then" => TokenKind::KwThen,
        "else" => TokenKind::KwElse,
        "true" => TokenKind::KwTrue,
        "false" => TokenKind::KwFalse,
        _ => return None,
    })
}

impl<'src> Iterator for Lexer<'src> {
    type Item = Result<Token, LexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.eof_emitted {
            return None;
        }

        self.skip_trivia();

        // A pending newline is only a statement separator if a real token
        // follows it; a newline run trailing straight into end of input is
        // dropped like leading trivia (see decision in the doc comment).
        let Some(&(start, ch)) = self.chars.peek() else {
            self.pending_newline = false;
            self.eof_emitted = true;
            let pos = self.source.len();
            return Some(Ok(Token::new(TokenKind::Eof, Span::new(pos, pos))));
        };

        if self.pending_newline && self.after_first_token {
            self.pending_newline = false;
            let start = self.pending_newline_start;
            return Some(Ok(Token::new(
                TokenKind::Newline,
                Span::new(start, start + 1),
            )));
        }
        self.pending_newline = false;

        self.after_first_token = true;

        if ch == '"' || ch == '\'' {
            return Some(self.scan_string(start, ch));
        }
        if ch.is_alphabetic() || ch == '_' {
            return Some(Ok(self.scan_ident(start)));
        }
        Some(self.scan_punct(start, ch))
    }
}

#[cfg(test)]
mod tests {
    use crate::Lexer;
    use crate::TokenKind;

    /// Collects the `TokenKind`s produced by lexing `source`, up to and
    /// including `Eof`. Panics on the first lexing error.
    fn lex_kinds(source: &str) -> Vec<TokenKind> {
        Lexer::new(source)
            .map(|result| result.expect("unexpected lex error"))
            .map(|token| token.kind)
            .collect()
    }

    #[test]
    fn lexes_beam_header() {
        let tokens: Vec<TokenKind> = lex_kinds("beam build {");
        assert_eq!(
            tokens,
            vec![
                TokenKind::KwBeam,
                TokenKind::Ident("build".into()),
                TokenKind::LBrace,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_string_and_operators() {
        let tokens = lex_kinds(r#"let x = env("PROFILE") == "release""#);
        assert!(tokens.contains(&TokenKind::Str("PROFILE".into())));
        assert!(tokens.contains(&TokenKind::EqEq));
    }

    #[test]
    fn reports_unterminated_string_with_span() {
        let err = Lexer::new("run \"oops").find_map(Result::err).unwrap();
        assert_eq!(err.span.start, 4);
        assert!(err.message.contains("unterminated string"));
    }

    #[test]
    fn skips_comments_and_collapses_newlines() {
        let tokens = lex_kinds("# comment\n\n\nbeam x {\n}");
        let newlines = tokens.iter().filter(|t| **t == TokenKind::Newline).count();
        assert_eq!(newlines, 1);
    }

    #[test]
    fn no_newline_token_before_eof() {
        // Trailing blank lines are trivia, not a statement separator: there
        // is nothing after them to separate from.
        let tokens = lex_kinds("beam x {\n}\n\n");
        assert_eq!(tokens.last(), Some(&TokenKind::Eof));
        assert_eq!(tokens[tokens.len() - 2], TokenKind::RBrace);
    }

    #[test]
    fn lexes_all_keywords() {
        let tokens = lex_kinds("version import as let default beam if then else true false hook");
        assert_eq!(
            tokens,
            vec![
                TokenKind::KwVersion,
                TokenKind::KwImport,
                TokenKind::KwAs,
                TokenKind::KwLet,
                TokenKind::KwDefault,
                TokenKind::KwBeam,
                TokenKind::KwIf,
                TokenKind::KwThen,
                TokenKind::KwElse,
                TokenKind::KwTrue,
                TokenKind::KwFalse,
                TokenKind::KwHook,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_both_string_delimiters() {
        let tokens = lex_kinds(r#""double" 'single'"#);
        assert_eq!(
            tokens,
            vec![
                TokenKind::Str("double".into()),
                TokenKind::Str("single".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn decodes_string_escapes() {
        let tokens = lex_kinds(r#""a\"b\\c\nd\te""#);
        assert_eq!(tokens[0], TokenKind::Str("a\"b\\c\nd\te".into()));
    }

    #[test]
    fn string_passes_through_interpolation_braces_as_raw_text() {
        let tokens = lex_kinds(r#""hello {name}""#);
        assert_eq!(tokens[0], TokenKind::Str("hello {name}".into()));
    }

    #[test]
    fn lexes_logical_operators_and_needs_colon_syntax() {
        let tokens = lex_kinds("a && b || c != d api:build");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::AndAnd,
                TokenKind::Ident("b".into()),
                TokenKind::OrOr,
                TokenKind::Ident("c".into()),
                TokenKind::NotEq,
                TokenKind::Ident("d".into()),
                TokenKind::Ident("api".into()),
                TokenKind::Colon,
                TokenKind::Ident("build".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn reports_unexpected_character() {
        let err = Lexer::new("beam @ {").find_map(Result::err).unwrap();
        assert_eq!(err.span, crate::Span::new(5, 6));
        assert!(err.message.contains('@'));
    }

    #[test]
    fn token_spans_are_accurate_byte_offsets() {
        let tokens: Vec<_> = Lexer::new("beam build")
            .map(|r| r.expect("unexpected lex error"))
            .collect();
        assert_eq!(tokens[0].span, crate::Span::new(0, 4)); // "beam"
        assert_eq!(tokens[1].span, crate::Span::new(5, 10)); // "build"
    }

    #[test]
    fn eof_span_is_at_end_of_source() {
        let tokens: Vec<_> = Lexer::new("x")
            .map(|r| r.expect("unexpected lex error"))
            .collect();
        assert_eq!(tokens.last().unwrap().span, crate::Span::new(1, 1));
    }
}
