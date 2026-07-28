//! Hand-written lexer: whitespace-separated words with POSIX quoting,
//! a small operator set, and hard errors for out-of-subset syntax.
//! Newlines lex as `;` so multi-line `run` strings behave like a
//! sequence of commands.

use crate::error::ShellParseError;
use crate::token::{Span, Token, TokenKind, Word, WordPart};

/// The suggestion attached to every "this construct is not supported"
/// error, shared so the copy stays consistent.
pub(crate) const OUT_OF_SUBSET_HELP: &str = "move the logic into a script invoked by `run`, or declare `executor system_shell` on this beam";

pub(crate) fn lex(source: &str) -> Result<Vec<Token>, ShellParseError> {
    lex_at(source, 0)
}

pub(crate) fn lex_at(source: &str, offset: usize) -> Result<Vec<Token>, ShellParseError> {
    Lexer {
        source,
        chars: source.char_indices().peekable(),
        offset,
    }
    .run()
}

/// A character that always ends a word: whitespace or one of the
/// operator characters. Everything else (including `{`, `$`, quotes,
/// and `\`) is handled inside `word`.
fn is_word_boundary(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | '&' | '|' | ';' | '>' | '<' | '(' | ')' | '`'
    )
}

struct Lexer<'a> {
    source: &'a str,
    chars: std::iter::Peekable<std::str::CharIndices<'a>>,
    offset: usize,
}

impl<'a> Lexer<'a> {
    fn run(mut self) -> Result<Vec<Token>, ShellParseError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_spaces();
            let Some(&(pos, ch)) = self.chars.peek() else {
                break;
            };
            let token = match ch {
                '\n' | ';' => {
                    self.chars.next();
                    Token {
                        kind: TokenKind::Semi,
                        span: self.span(pos, pos + 1),
                    }
                }
                '|' => self.lex_pipe(pos),
                '&' => self.lex_and(pos)?,
                '>' => self.lex_redirect_out(pos),
                '<' => self.lex_redirect_in(pos)?,
                '(' | ')' => {
                    return Err(self.error(
                        pos,
                        pos + 1,
                        "subshells and function definitions are not supported",
                        Some(OUT_OF_SUBSET_HELP),
                    ));
                }
                '`' => {
                    return Err(self.error(
                        pos,
                        pos + 1,
                        "backquote command substitution is not supported",
                        Some("use `$(...)` instead"),
                    ));
                }
                digit @ ('1' | '2') if self.looks_like_fd_redirect(pos, digit) => {
                    self.fd_redirect()
                }
                _ => {
                    let word = self.word()?;
                    Token {
                        span: word.span,
                        kind: TokenKind::Word(word),
                    }
                }
            };
            tokens.push(token);
        }
        Ok(tokens)
    }

    fn pos(&mut self) -> usize {
        self.chars.peek().map_or(self.source.len(), |&(i, _)| i)
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span {
            start: start + self.offset,
            end: end + self.offset,
        }
    }

    fn error(
        &self,
        start: usize,
        end: usize,
        message: impl Into<String>,
        suggestion: Option<&str>,
    ) -> ShellParseError {
        ShellParseError {
            message: message.into(),
            span: self.span(start, end),
            suggestion: suggestion.map(str::to_string),
        }
    }

    fn skip_spaces(&mut self) {
        while matches!(self.chars.peek().copied(), Some((_, ' ' | '\t'))) {
            self.chars.next();
        }
    }

    fn lex_pipe(&mut self, pos: usize) -> Token {
        self.chars.next();
        if matches!(self.chars.peek().copied(), Some((_, '|'))) {
            self.chars.next();
            return Token {
                kind: TokenKind::OrOr,
                span: self.span(pos, pos + 2),
            };
        }
        Token {
            kind: TokenKind::Pipe,
            span: self.span(pos, pos + 1),
        }
    }

    fn lex_and(&mut self, pos: usize) -> Result<Token, ShellParseError> {
        self.chars.next();
        if matches!(self.chars.peek().copied(), Some((_, '&'))) {
            self.chars.next();
            return Ok(Token {
                kind: TokenKind::AndAnd,
                span: self.span(pos, pos + 2),
            });
        }
        Err(self.error(
            pos,
            pos + 1,
            "background jobs (`&`) are not supported",
            Some("the engine already parallelizes beams; run one command per `run` entry"),
        ))
    }

    fn lex_redirect_out(&mut self, pos: usize) -> Token {
        self.chars.next();
        if matches!(self.chars.peek().copied(), Some((_, '>'))) {
            self.chars.next();
            return Token {
                kind: TokenKind::RedirectOut {
                    stderr: false,
                    append: true,
                },
                span: self.span(pos, pos + 2),
            };
        }
        Token {
            kind: TokenKind::RedirectOut {
                stderr: false,
                append: false,
            },
            span: self.span(pos, pos + 1),
        }
    }

    fn lex_redirect_in(&mut self, pos: usize) -> Result<Token, ShellParseError> {
        self.chars.next();
        if matches!(self.chars.peek().copied(), Some((_, '<'))) {
            self.chars.next();
            return Err(self.error(
                pos,
                pos + 2,
                "heredocs are not supported",
                Some(OUT_OF_SUBSET_HELP),
            ));
        }
        Ok(Token {
            kind: TokenKind::RedirectIn,
            span: self.span(pos, pos + 1),
        })
    }

    /// True when `digit` (`1` or `2`) is immediately followed by `>`,
    /// i.e. it names a file descriptor rather than starting a word.
    fn looks_like_fd_redirect(&self, pos: usize, digit: char) -> bool {
        (digit == '1' || digit == '2') && self.source[pos + 1..].starts_with('>')
    }

    /// Consumes a leading `1` or `2` already confirmed (by
    /// `looks_like_fd_redirect`) to be glued to a following `>`.
    fn fd_redirect(&mut self) -> Token {
        let (pos, digit) = self.chars.next().expect("caller confirmed a fd digit");
        self.chars.next(); // the '>'
        let stderr = digit == '2';
        if matches!(self.chars.peek().copied(), Some((_, '>'))) {
            self.chars.next();
            let end = self.pos();
            return Token {
                kind: TokenKind::RedirectOut {
                    stderr,
                    append: true,
                },
                span: self.span(pos, end),
            };
        }
        if stderr && self.source[self.pos()..].starts_with("&1") {
            self.chars.next();
            self.chars.next();
            let end = self.pos();
            return Token {
                kind: TokenKind::StderrToStdout,
                span: self.span(pos, end),
            };
        }
        let end = self.pos();
        Token {
            kind: TokenKind::RedirectOut {
                stderr,
                append: false,
            },
            span: self.span(pos, end),
        }
    }

    /// Scans a word: text, quoting, variables, command substitution and
    /// brace-expansion detection, until whitespace or an operator ends it.
    fn word(&mut self) -> Result<Word, ShellParseError> {
        let start = self.pos();
        let mut parts = Vec::new();
        let mut text = String::new();
        while let Some(&(pos, ch)) = self.chars.peek() {
            if is_word_boundary(ch) {
                break;
            }
            match ch {
                '\\' => self.escape(&mut text)?,
                '\'' => self.single_quoted(&mut parts, &mut text)?,
                '"' => {
                    self.flush_text(&mut text, &mut parts);
                    let inner = self.double_quoted()?;
                    parts.push(WordPart::DoubleQuoted(inner));
                }
                '$' => match self.dollar()? {
                    Some(part) => {
                        self.flush_text(&mut text, &mut parts);
                        parts.push(part);
                    }
                    None => text.push('$'),
                },
                '{' => {
                    self.check_brace_expansion(pos)?;
                    text.push('{');
                    self.chars.next();
                }
                _ => {
                    text.push(ch);
                    self.chars.next();
                }
            }
        }
        self.flush_text(&mut text, &mut parts);
        let end = self.pos();
        Ok(Word {
            parts,
            span: self.span(start, end),
        })
    }

    fn flush_text(&self, text: &mut String, parts: &mut Vec<WordPart>) {
        if !text.is_empty() {
            parts.push(WordPart::Text(std::mem::take(text)));
        }
    }

    /// Unquoted `\x`: `x` joins the current text literally. A trailing
    /// backslash at end of input is an error.
    fn escape(&mut self, text: &mut String) -> Result<(), ShellParseError> {
        let (bs_pos, _) = self.chars.next().expect("caller peeked '\\'");
        match self.chars.next() {
            Some((_, ch)) => {
                text.push(ch);
                Ok(())
            }
            None => Err(self.error(
                bs_pos,
                bs_pos + 1,
                "incomplete escape at end of command",
                None,
            )),
        }
    }

    fn single_quoted(
        &mut self,
        parts: &mut Vec<WordPart>,
        text: &mut String,
    ) -> Result<(), ShellParseError> {
        self.flush_text(text, parts);
        let (quote_pos, _) = self.chars.next().expect("caller peeked '\\''");
        let mut value = String::new();
        loop {
            match self.chars.next() {
                Some((_, '\'')) => break,
                Some((_, ch)) => value.push(ch),
                None => {
                    return Err(self.error(
                        quote_pos,
                        quote_pos + 1,
                        "unclosed single quote",
                        None,
                    ));
                }
            }
        }
        parts.push(WordPart::SingleQuoted(value));
        Ok(())
    }

    /// `"..."`: literal text, with `$NAME`/`${NAME}`/`$(...)` staying
    /// live and `\"`, `\\`, `\$` as the only recognized escapes.
    fn double_quoted(&mut self) -> Result<Vec<WordPart>, ShellParseError> {
        let (quote_pos, _) = self.chars.next().expect("caller peeked '\"'");
        let mut parts = Vec::new();
        let mut text = String::new();
        loop {
            match self.chars.peek().copied() {
                None => {
                    return Err(self.error(
                        quote_pos,
                        quote_pos + 1,
                        "unclosed double quote",
                        None,
                    ));
                }
                Some((_, '"')) => {
                    self.chars.next();
                    break;
                }
                Some((_, '\\')) => self.double_quoted_escape(&mut text, quote_pos)?,
                Some((_, '$')) => match self.dollar()? {
                    Some(part) => {
                        self.flush_text(&mut text, &mut parts);
                        parts.push(part);
                    }
                    None => text.push('$'),
                },
                Some((_, ch)) => {
                    text.push(ch);
                    self.chars.next();
                }
            }
        }
        self.flush_text(&mut text, &mut parts);
        Ok(parts)
    }

    fn double_quoted_escape(
        &mut self,
        text: &mut String,
        quote_pos: usize,
    ) -> Result<(), ShellParseError> {
        self.chars.next(); // the backslash
        match self.chars.next() {
            Some((_, c @ ('"' | '\\' | '$'))) => text.push(c),
            Some((_, c)) => {
                text.push('\\');
                text.push(c);
            }
            None => {
                return Err(self.error(quote_pos, quote_pos + 1, "unclosed double quote", None));
            }
        }
        Ok(())
    }

    /// Called with the lexer positioned at `$`. Returns the expansion it
    /// introduces, or `None` when the `$` is bare (no name, `{`, or `(`
    /// follows) and therefore literal.
    fn dollar(&mut self) -> Result<Option<WordPart>, ShellParseError> {
        let (dollar_pos, _) = self.chars.next().expect("caller peeked '$'");
        match self.chars.peek().copied() {
            Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {
                Ok(Some(WordPart::Var(self.scan_ident())))
            }
            Some((_, '{')) => {
                self.chars.next();
                self.braced_var(dollar_pos).map(Some)
            }
            Some((_, '(')) => {
                self.chars.next();
                if matches!(self.chars.peek().copied(), Some((_, '('))) {
                    self.chars.next();
                    let end = self.pos();
                    return Err(self.error(
                        dollar_pos,
                        end,
                        "arithmetic expansion is not supported",
                        Some(OUT_OF_SUBSET_HELP),
                    ));
                }
                self.command_substitution(dollar_pos).map(Some)
            }
            _ => Ok(None),
        }
    }

    fn scan_ident(&mut self) -> String {
        let mut name = String::new();
        while let Some(&(_, c)) = self.chars.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                name.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        name
    }

    /// `${` already consumed: requires `NAME}` with nothing else, since
    /// anything richer (`${VAR:-default}`, `${#VAR}`, ...) is out of
    /// subset.
    fn braced_var(&mut self, dollar_pos: usize) -> Result<WordPart, ShellParseError> {
        let starts_ident = matches!(self.chars.peek().copied(), Some((_, c)) if c.is_ascii_alphabetic() || c == '_');
        if starts_ident {
            let name = self.scan_ident();
            if matches!(self.chars.peek().copied(), Some((_, '}'))) {
                self.chars.next();
                return Ok(WordPart::Var(name));
            }
        }
        let end = self
            .chars
            .peek()
            .copied()
            .map_or(self.pos(), |(p, c)| p + c.len_utf8());
        Err(self.error(
            dollar_pos,
            end,
            "advanced parameter expansions like `${VAR:-default}` are not supported",
            Some(OUT_OF_SUBSET_HELP),
        ))
    }

    /// `$(` already consumed: captures the raw inner source up to the
    /// matching `)`, tracking paren depth and both quote kinds so nested
    /// substitutions and quoted parens do not close it early.
    fn command_substitution(&mut self, dollar_pos: usize) -> Result<WordPart, ShellParseError> {
        let inner_start = self.pos();
        let mut depth = 1u32;
        let mut in_single = false;
        let mut in_double = false;
        loop {
            match self.chars.next() {
                None => {
                    return Err(self.error(
                        dollar_pos,
                        inner_start,
                        "unclosed command substitution",
                        None,
                    ));
                }
                Some((_, '\\')) if in_double => {
                    self.chars.next();
                }
                Some((_, '\'')) if !in_double => in_single = !in_single,
                Some((_, '"')) if !in_single => in_double = !in_double,
                Some((_, '(')) if !in_single && !in_double => depth += 1,
                Some((pos, ')')) if !in_single && !in_double => {
                    depth -= 1;
                    if depth == 0 {
                        let source = self.source[inner_start..pos].to_string();
                        return Ok(WordPart::CmdSubst {
                            source,
                            span: self.span(inner_start, pos),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    /// An unquoted `{` at `brace_pos` is brace expansion only when a
    /// matching `}` follows on the same word with a `,` in between and
    /// no whitespace; otherwise it is literal.
    fn check_brace_expansion(&self, brace_pos: usize) -> Result<(), ShellParseError> {
        let rest = &self.source[brace_pos + 1..];
        let mut has_comma = false;
        let mut close_at = None;
        for (i, c) in rest.char_indices() {
            match c {
                '}' => {
                    close_at = Some(i);
                    break;
                }
                ',' => has_comma = true,
                c if c.is_whitespace() => break,
                _ => {}
            }
        }
        match close_at {
            Some(i) if has_comma => Err(self.error(
                brace_pos,
                brace_pos + 1 + i + 1,
                "brace expansion is not supported",
                Some("spell the alternatives out as separate arguments"),
            )),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{TokenKind, WordPart};

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn err(src: &str) -> crate::ShellParseError {
        lex(src).unwrap_err()
    }

    fn word_parts(kind: &TokenKind) -> &[WordPart] {
        match kind {
            TokenKind::Word(w) => &w.parts,
            other => panic!("expected word, got {other:?}"),
        }
    }

    #[test]
    fn splits_plain_words_on_whitespace() {
        let kinds = kinds("echo hello world");
        assert_eq!(kinds.len(), 3);
        assert_eq!(word_parts(&kinds[0]), &[WordPart::Text("echo".into())]);
    }

    #[test]
    fn recognizes_operators() {
        insta::assert_debug_snapshot!(kinds("a && b || c | d ; e"));
    }

    #[test]
    fn newline_lexes_like_a_semicolon() {
        assert_eq!(kinds("a\nb"), kinds("a;b"));
    }

    #[test]
    fn recognizes_redirections() {
        insta::assert_debug_snapshot!(kinds("a > f >> g < h 1> i 1>> j 2> k 2>> l 2>&1"));
    }

    #[test]
    fn a_digit_not_glued_to_a_redirect_stays_a_word() {
        // `echo 2 > f`: the 2 is an argument, not a file descriptor.
        insta::assert_debug_snapshot!(kinds("echo 2 > f"));
    }

    #[test]
    fn single_quotes_are_literal() {
        let kinds = kinds("echo 'a $b *'");
        assert_eq!(
            word_parts(&kinds[1]),
            &[WordPart::SingleQuoted("a $b *".into())]
        );
    }

    #[test]
    fn double_quotes_keep_vars_live() {
        insta::assert_debug_snapshot!(kinds(r#"echo "a $b ${c} $(d e)""#));
    }

    #[test]
    fn backslash_escapes_the_next_character_unquoted() {
        let kinds = kinds(r"echo a\ b");
        assert_eq!(kinds.len(), 2);
        assert_eq!(word_parts(&kinds[1]), &[WordPart::Text("a b".into())]);
    }

    #[test]
    fn dollar_without_a_name_is_literal() {
        let kinds = kinds("echo $ $1x");
        // `$` alone and `$1` (digits are not variable names) stay literal.
        insta::assert_debug_snapshot!(kinds);
    }

    #[test]
    fn adjacent_parts_form_one_word() {
        let kinds = kinds(r#"a$B'c'"d""#);
        assert_eq!(kinds.len(), 1);
        insta::assert_debug_snapshot!(word_parts(&kinds[0]));
    }

    #[test]
    fn command_substitution_captures_raw_source_with_balanced_parens() {
        let kinds = kinds("echo $(f $(g) ')')");
        match &word_parts(&kinds[1])[0] {
            WordPart::CmdSubst { source, .. } => assert_eq!(source, "f $(g) ')'"),
            other => panic!("expected cmd subst, got {other:?}"),
        }
    }

    #[test]
    fn spans_are_byte_offsets_into_the_source() {
        let tokens = lex("ab && cd").unwrap();
        assert_eq!((tokens[0].span.start, tokens[0].span.end), (0, 2));
        assert_eq!((tokens[1].span.start, tokens[1].span.end), (3, 5));
        assert_eq!((tokens[2].span.start, tokens[2].span.end), (6, 8));
    }

    #[test]
    fn lex_at_shifts_spans() {
        let tokens = lex_at("ab", 10).unwrap();
        assert_eq!((tokens[0].span.start, tokens[0].span.end), (10, 12));
    }

    // Out-of-subset syntax: every case is (source, snapshot of message,
    // span, and suggestion). Snapshot the full rendered diagnostic so the
    // copy is reviewed like a product feature.
    #[test]
    fn rejects_background_jobs() {
        insta::assert_snapshot!(err("watch & build").render("watch & build"));
    }

    #[test]
    fn rejects_subshells() {
        insta::assert_snapshot!(err("(cd sub; make)").render("(cd sub; make)"));
    }

    #[test]
    fn rejects_heredocs() {
        insta::assert_snapshot!(err("cat << EOF").render("cat << EOF"));
    }

    #[test]
    fn rejects_backquote_substitution() {
        insta::assert_snapshot!(err("echo `date`").render("echo `date`"));
    }

    #[test]
    fn rejects_arithmetic_expansion() {
        insta::assert_snapshot!(err("echo $((1+2))").render("echo $((1+2))"));
    }

    #[test]
    fn rejects_advanced_parameter_expansion() {
        insta::assert_snapshot!(err("echo ${X:-y}").render("echo ${X:-y}"));
    }

    #[test]
    fn rejects_brace_expansion() {
        insta::assert_snapshot!(err("touch f.{rs,md}").render("touch f.{rs,md}"));
    }

    #[test]
    fn plain_braces_are_literal() {
        // `find -exec {} +` and `{x}` must keep working: only an unquoted
        // brace group containing a comma is brace expansion.
        let kinds = kinds("find . -exec rm {} +");
        assert_eq!(kinds.len(), 6);
    }

    #[test]
    fn rejects_unclosed_single_quote() {
        insta::assert_snapshot!(err("echo 'oops").render("echo 'oops"));
    }

    #[test]
    fn rejects_unclosed_double_quote() {
        insta::assert_snapshot!(err("echo \"oops").render("echo \"oops"));
    }

    #[test]
    fn rejects_unclosed_command_substitution() {
        insta::assert_snapshot!(err("echo $(true").render("echo $(true"));
    }
}
