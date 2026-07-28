//! Recursive-descent parser: turns the lexer's flat token list into a
//! `Program`. Grammar (newline already lexes as `;`, see `lexer`):
//!
//! ```text
//! program  := and_or ((';') and_or)*        // empty items skipped
//! and_or   := pipeline (('&&' | '||') pipeline)*
//! pipeline := ['!'] command ('|' command)*
//! command  := assignment* (word | redirect)+
//! ```

use crate::ast::{self, AndOrList, AndOrOp, Assignment, Command, Pipeline, Program, Redirect};
use crate::error::ShellParseError;
use crate::lexer::{self, OUT_OF_SUBSET_HELP};
use crate::token::{self, Span, Token, TokenKind};

/// Parses a whole shell command line.
pub fn parse(source: &str) -> Result<Program, ShellParseError> {
    parse_at(source, 0)
}

/// Parses `source` as if it started at byte `offset` of some larger
/// string, so spans in errors and in the AST stay absolute. Used both
/// for the top-level parse and, recursively, for command substitution.
fn parse_at(source: &str, offset: usize) -> Result<Program, ShellParseError> {
    let tokens = lexer::lex_at(source, offset)?;
    Parser { tokens, pos: 0 }.parse_program()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|token| &token.kind)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn parse_program(&mut self) -> Result<Program, ShellParseError> {
        let mut items = Vec::new();
        loop {
            while matches!(self.peek_kind(), Some(TokenKind::Semi)) {
                self.advance();
            }
            if self.peek().is_none() {
                break;
            }
            items.push(self.parse_and_or()?);
            match self.peek_kind() {
                Some(TokenKind::Semi) => {
                    self.advance();
                }
                None => break,
                Some(_) => {
                    // The grammar below only ever stops an and/or chain on
                    // `;` or end of input; anything else here would be a
                    // parser bug rather than a user mistake, but we still
                    // report it as a diagnostic instead of panicking.
                    let token = self.peek().expect("checked above");
                    return Err(ShellParseError {
                        message: "unexpected token".to_string(),
                        span: token.span,
                        suggestion: None,
                    });
                }
            }
        }
        Ok(Program { items })
    }

    fn parse_and_or(&mut self) -> Result<AndOrList, ShellParseError> {
        let first = self.require_pipeline()?;
        let mut rest = Vec::new();
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::AndAnd) => AndOrOp::And,
                Some(TokenKind::OrOr) => AndOrOp::Or,
                _ => break,
            };
            let op_token = self.advance().expect("peeked the operator above");
            let op_text = if op == AndOrOp::And { "&&" } else { "||" };
            let pipeline = self.require_pipeline_after(op_token.span, op_text)?;
            rest.push((op, pipeline));
        }
        Ok(AndOrList { first, rest })
    }

    /// A pipeline required at the start of an and/or list: no preceding
    /// operator to blame, so the error points at whatever unexpected
    /// token stopped it.
    fn require_pipeline(&mut self) -> Result<Pipeline, ShellParseError> {
        match self.parse_pipeline()? {
            Some(pipeline) => Ok(pipeline),
            None => Err(self.expected_a_command_error()),
        }
    }

    /// A pipeline required right after `&&`/`||`: the error blames that
    /// operator's span when nothing follows it.
    fn require_pipeline_after(
        &mut self,
        op_span: Span,
        op_text: &str,
    ) -> Result<Pipeline, ShellParseError> {
        match self.parse_pipeline()? {
            Some(pipeline) => Ok(pipeline),
            None => Err(ShellParseError {
                message: format!("expected a command after `{op_text}`"),
                span: op_span,
                suggestion: None,
            }),
        }
    }

    fn expected_a_command_error(&self) -> ShellParseError {
        let span = self
            .peek()
            .map_or(Span { start: 0, end: 0 }, |token| token.span);
        ShellParseError {
            message: "expected a command".to_string(),
            span,
            suggestion: None,
        }
    }

    fn parse_pipeline(&mut self) -> Result<Option<Pipeline>, ShellParseError> {
        let bang_span = self.consume_negation();
        let negated = bang_span.is_some();
        let first = match self.parse_command()? {
            Some(command) => command,
            None => {
                return match bang_span {
                    Some(span) => Err(ShellParseError {
                        message: "expected a command after `!`".to_string(),
                        span,
                        suggestion: None,
                    }),
                    None => Ok(None),
                };
            }
        };
        let mut commands = vec![first];
        loop {
            if !matches!(self.peek_kind(), Some(TokenKind::Pipe)) {
                break;
            }
            let pipe_token = self.advance().expect("peeked the pipe above");
            let command = match self.parse_command()? {
                Some(command) => command,
                None => {
                    return Err(ShellParseError {
                        message: "expected a command after `|`".to_string(),
                        span: pipe_token.span,
                        suggestion: None,
                    });
                }
            };
            commands.push(command);
        }
        Ok(Some(Pipeline { negated, commands }))
    }

    /// A word whose parts are exactly `[Text("!")]` at pipeline
    /// position negates the pipeline's status.
    fn consume_negation(&mut self) -> Option<Span> {
        let Some(Token {
            kind: TokenKind::Word(word),
            span,
        }) = self.peek()
        else {
            return None;
        };
        if word.parts.len() == 1 && matches!(&word.parts[0], token::WordPart::Text(t) if t == "!") {
            let span = *span;
            self.advance();
            return Some(span);
        }
        None
    }

    /// `command := assignment* (word | redirect)+`. Returns `None` when
    /// no command token was found at all (used by callers to detect a
    /// missing command after an operator).
    fn parse_command(&mut self) -> Result<Option<Command>, ShellParseError> {
        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        let mut seen_word = false;
        let mut span: Option<Span> = None;

        while let Some(token) = self.peek().cloned() {
            let token_span = token.span;
            match token.kind {
                TokenKind::Word(word) => {
                    if !seen_word {
                        if let Some((name, value)) = try_assignment(&word)? {
                            self.advance();
                            extend(&mut span, token_span);
                            assignments.push(Assignment { name, value });
                            continue;
                        }
                        if let Some(message) = reserved_word_message(&word) {
                            return Err(ShellParseError {
                                message,
                                span: token_span,
                                suggestion: Some(OUT_OF_SUBSET_HELP.to_string()),
                            });
                        }
                    }
                    self.advance();
                    seen_word = true;
                    extend(&mut span, token_span);
                    words.push(convert_word(token::Word {
                        parts: word.parts,
                        span: token_span,
                    })?);
                }
                TokenKind::RedirectOut { stderr, append } => {
                    self.advance();
                    let target =
                        self.expect_redirect_target(token_span, redirect_out_text(stderr, append))?;
                    extend(&mut span, token_span);
                    extend(&mut span, target.span);
                    redirects.push(Redirect::Out {
                        stderr,
                        append,
                        target,
                    });
                }
                TokenKind::RedirectIn => {
                    self.advance();
                    let target = self.expect_redirect_target(token_span, "<")?;
                    extend(&mut span, token_span);
                    extend(&mut span, target.span);
                    redirects.push(Redirect::In { target });
                }
                TokenKind::StderrToStdout => {
                    self.advance();
                    extend(&mut span, token_span);
                    redirects.push(Redirect::StderrToStdout);
                }
                _ => break,
            }
        }

        if assignments.is_empty() && words.is_empty() && redirects.is_empty() {
            return Ok(None);
        }

        Ok(Some(Command {
            assignments,
            words,
            redirects,
            span: span.expect("a non-empty command has at least one span-contributing token"),
        }))
    }

    /// A redirect target is exactly one word; anything else (nothing
    /// left, or another operator) is a missing-target error blaming the
    /// redirect operator's own span.
    fn expect_redirect_target(
        &mut self,
        op_span: Span,
        op_text: &str,
    ) -> Result<ast::Word, ShellParseError> {
        if let Some(Token {
            kind: TokenKind::Word(_),
            ..
        }) = self.peek()
        {
            let token = self.advance().expect("peeked a word above");
            let TokenKind::Word(word) = token.kind else {
                unreachable!("peeked a word above")
            };
            return convert_word(token::Word {
                parts: word.parts,
                span: token.span,
            });
        }
        Err(ShellParseError {
            message: format!("expected a file after `{op_text}`"),
            span: op_span,
            suggestion: None,
        })
    }
}

/// Grows `acc` to also cover `span`, or seeds it when this is the first
/// contributing span.
fn extend(acc: &mut Option<Span>, span: Span) {
    *acc = Some(match *acc {
        Some(current) => Span {
            start: current.start,
            end: span.end,
        },
        None => span,
    });
}

/// The display form of a `>`-family redirect, used in "expected a file
/// after `X`" diagnostics.
fn redirect_out_text(stderr: bool, append: bool) -> &'static str {
    match (stderr, append) {
        (false, false) => ">",
        (false, true) => ">>",
        (true, false) => "2>",
        (true, true) => "2>>",
    }
}

/// At command position (before any word of the command has been seen),
/// a word matching a shell keyword is out of subset. `None` means the
/// word is an ordinary argument.
fn reserved_word_message(word: &token::Word) -> Option<String> {
    if word.parts.len() != 1 {
        return None;
    }
    let token::WordPart::Text(text) = &word.parts[0] else {
        return None;
    };
    match text.as_str() {
        "if" | "then" | "elif" | "else" | "fi" => {
            Some("`if` conditionals are not supported by the embedded shell".to_string())
        }
        "for" => Some("`for` loops are not supported by the embedded shell".to_string()),
        "while" | "until" => {
            Some("`while` loops are not supported by the embedded shell".to_string())
        }
        "case" | "esac" => {
            Some("`case` statements are not supported by the embedded shell".to_string())
        }
        "function" => Some("shell functions are not supported by the embedded shell".to_string()),
        "do" | "done" | "in" => Some(format!(
            "`{text}` is a shell keyword the embedded shell does not support"
        )),
        _ => None,
    }
}

/// Recognizes `NAME=value` at command start: the word's first part must
/// be unquoted text matching `^[A-Za-z_][A-Za-z0-9_]*=`. The value is
/// the remainder of that text part plus every part after it.
fn try_assignment(word: &token::Word) -> Result<Option<(String, ast::Word)>, ShellParseError> {
    let Some(token::WordPart::Text(first_text)) = word.parts.first() else {
        return Ok(None);
    };
    let Some((name, remainder)) = split_assignment_name(first_text) else {
        return Ok(None);
    };

    let mut parts = Vec::new();
    if !remainder.is_empty() {
        parts.push(ast::WordPart::Text(remainder.to_string()));
    }
    for part in &word.parts[1..] {
        parts.push(convert_part(part.clone())?);
    }

    let value_start = word.span.start + name.len() + 1;
    Ok(Some((
        name.to_string(),
        ast::Word {
            parts,
            span: Span {
                start: value_start,
                end: word.span.end,
            },
        },
    )))
}

/// Splits `NAME=rest` at the first `=`, requiring `NAME` to look like a
/// shell identifier. Returns `None` when the text does not match.
fn split_assignment_name(text: &str) -> Option<(&str, &str)> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return None;
    }
    for (i, c) in chars {
        if c == '=' {
            return Some((&text[..i], &text[i + 1..]));
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
    }
    None
}

fn convert_word(word: token::Word) -> Result<ast::Word, ShellParseError> {
    Ok(ast::Word {
        parts: convert_parts(word.parts)?,
        span: word.span,
    })
}

fn convert_parts(parts: Vec<token::WordPart>) -> Result<Vec<ast::WordPart>, ShellParseError> {
    parts.into_iter().map(convert_part).collect()
}

fn convert_part(part: token::WordPart) -> Result<ast::WordPart, ShellParseError> {
    Ok(match part {
        token::WordPart::Text(t) => ast::WordPart::Text(t),
        token::WordPart::SingleQuoted(t) => ast::WordPart::SingleQuoted(t),
        token::WordPart::DoubleQuoted(inner) => ast::WordPart::DoubleQuoted(convert_parts(inner)?),
        token::WordPart::Var(name) => ast::WordPart::Var(name),
        token::WordPart::CmdSubst { source, span } => {
            let program = parse_at(&source, span.start)?;
            ast::WordPart::CmdSubst(Box::new(program))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ast(src: &str) -> crate::ast::Program {
        parse(src).unwrap()
    }

    fn err(src: &str) -> String {
        parse(src).unwrap_err().render(src)
    }

    #[test]
    fn parses_a_simple_command() {
        insta::assert_debug_snapshot!(ast("cargo build --release"));
    }

    #[test]
    fn parses_sequencing_and_and_or() {
        insta::assert_debug_snapshot!(ast("a; b && c || d"));
    }

    #[test]
    fn and_and_or_are_left_associative_and_equal_precedence() {
        // POSIX: `a && b || c` is `(a && b) || c`.
        insta::assert_debug_snapshot!(ast("a && b || c && d"));
    }

    #[test]
    fn parses_a_pipeline() {
        insta::assert_debug_snapshot!(ast("cat f | grep x | wc -l"));
    }

    #[test]
    fn parses_negation() {
        let program = ast("! grep -q TODO src");
        assert!(program.items[0].first.negated);
    }

    #[test]
    fn parses_redirections_in_any_position() {
        insta::assert_debug_snapshot!(ast("cmd > out.txt 2>&1 < in.txt"));
    }

    #[test]
    fn parses_assignment_prefixes() {
        insta::assert_debug_snapshot!(ast("RUST_LOG=debug FOO=$bar cargo test"));
    }

    #[test]
    fn an_assignment_alone_is_a_command_without_words() {
        let program = ast("GREETING='hello world'");
        let command = &program.items[0].first.commands[0];
        assert_eq!(command.assignments.len(), 1);
        assert!(command.words.is_empty());
    }

    #[test]
    fn an_equals_sign_past_the_first_word_is_literal() {
        // `env A=b` passes `A=b` as an argument, not an assignment.
        let program = ast("env A=b");
        let command = &program.items[0].first.commands[0];
        assert!(command.assignments.is_empty());
        assert_eq!(command.words.len(), 2);
    }

    #[test]
    fn parses_command_substitution_recursively() {
        insta::assert_debug_snapshot!(ast("echo $(git rev-parse HEAD | cut -c1-7)"));
    }

    #[test]
    fn an_empty_source_is_an_empty_program() {
        assert!(ast("").items.is_empty());
        assert!(ast("  \n ; ; \n").items.is_empty());
    }

    #[test]
    fn rejects_if_conditionals() {
        insta::assert_snapshot!(err("if true; then echo y; fi"));
    }

    #[test]
    fn rejects_for_loops() {
        insta::assert_snapshot!(err("for f in *.rs; do echo $f; done"));
    }

    #[test]
    fn rejects_while_loops() {
        insta::assert_snapshot!(err("while true; do sleep 1; done"));
    }

    #[test]
    fn rejects_case_statements() {
        insta::assert_snapshot!(err("case $x in a) echo a;; esac"));
    }

    #[test]
    fn rejects_function_definitions() {
        insta::assert_snapshot!(err("greet() { echo hi; }"));
    }

    #[test]
    fn reserved_words_are_only_reserved_at_command_position() {
        // `echo if` is fine: `if` is an argument there.
        assert!(parse("echo if then done").is_ok());
    }

    #[test]
    fn quoting_a_reserved_word_makes_it_ordinary() {
        assert!(parse("'if' --version").is_ok());
    }

    #[test]
    fn rejects_a_trailing_operator() {
        insta::assert_snapshot!(err("a &&"));
    }

    #[test]
    fn rejects_a_redirect_without_target() {
        insta::assert_snapshot!(err("echo hi >"));
    }

    #[test]
    fn an_out_of_subset_error_inside_command_substitution_points_at_the_outer_source() {
        let error = parse("echo $(for x in a; do echo $x; done)").unwrap_err();
        assert!(
            error.span.start >= 7,
            "span must be inside the substitution"
        );
    }
}
