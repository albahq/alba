//! The lexer's output: operators and words. A word is a sequence of
//! parts because quoting decides, later, what expands and what globs:
//! `a$B'c'` is one word of three parts.

/// A byte range into the command source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Word(Word),
    AndAnd,
    OrOr,
    Pipe,
    Semi,
    /// `>`/`>>` (stderr: false) and `2>`/`2>>` (stderr: true). `1>` and
    /// `1>>` lex as stderr: false: the digit names the fd POSIX-style.
    RedirectOut {
        stderr: bool,
        append: bool,
    },
    RedirectIn,
    /// `2>&1`.
    StderrToStdout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub parts: Vec<WordPart>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WordPart {
    /// Unquoted text. Glob characters and `~` live here; expansion
    /// decides what they mean.
    Text(String),
    /// One character that a backslash escaped outside of quotes (`\ `,
    /// `\*`, `\\`). It is a separate part rather than folded into `Text`
    /// precisely so expansion can still tell it apart: an escaped
    /// character is literal, so it must neither field-split nor act as
    /// glob syntax, exactly like a quoted one.
    Escaped(char),
    SingleQuoted(String),
    /// `"..."`: only `Text`, `Var`, and `CmdSubst` appear inside.
    DoubleQuoted(Vec<WordPart>),
    /// `$NAME` or `${NAME}`.
    Var(String),
    /// `$(...)`: the raw inner source, parsed recursively by the parser.
    /// The span covers the inner source in the outer string, so nested
    /// diagnostics point at the right place.
    CmdSubst {
        source: String,
        span: Span,
    },
}
