//! Token types produced by the lexer.

/// A byte-offset span into the original source text.
///
/// `start` is inclusive, `end` is exclusive. Spans are used to report
/// diagnostics (see the `alba-diagnostics` crate) with a caret under the
/// exact offending text, so accuracy here matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// Creates a new span from a byte-offset range `[start, end)`.
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// The kind of a lexical token in the Beamfile DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// An identifier, e.g. a beam name or a variable name.
    Ident(String),
    /// The decoded content of a string literal, after processing escapes.
    /// Interpolation braces (`{`, `}`) are left as raw text; a later parsing
    /// stage re-scans this content to resolve interpolation.
    Str(String),

    // Keywords.
    KwVersion,
    KwImport,
    KwAs,
    KwLet,
    KwDefault,
    KwBeam,
    KwIf,
    KwThen,
    KwElse,
    KwTrue,
    KwFalse,
    KwHook,

    // Punctuation and operators.
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Colon,
    /// `=`
    Eq,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    Plus,
    /// `.`, member access (`git.branch`).
    Dot,

    /// A statement separator. Emitted only strictly between two other
    /// tokens: leading and trailing newline trivia (including blank lines
    /// and comments) are dropped, and a run of consecutive newlines
    /// collapses into a single `Newline` token.
    Newline,
    /// End of input. Yielded exactly once, as the last token.
    Eof,
}

/// A single lexical token together with its span in the source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    /// Creates a new token from its kind and span.
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}
