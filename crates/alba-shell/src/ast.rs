//! The parser's output: a fully parsed program tree. Mirrors the token
//! layer, except `WordPart::CmdSubst` is recursively parsed into its own
//! `Program` instead of carrying raw source.

use crate::token::Span;

/// A sequence of and/or lists, one per `;` or newline separated item.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub items: Vec<AndOrList>,
}

/// A pipeline followed by zero or more `&&`/`||`-joined pipelines,
/// left-associative and evaluated at equal precedence.
#[derive(Debug, Clone, PartialEq)]
pub struct AndOrList {
    pub first: Pipeline,
    pub rest: Vec<(AndOrOp, Pipeline)>,
}

/// The operator joining two pipelines in an `AndOrList`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndOrOp {
    And,
    Or,
}

/// One or more commands joined by `|`, optionally negated with a
/// leading `!`.
#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub negated: bool,
    pub commands: Vec<Command>,
}

/// A single command: leading assignments, its words, and its
/// redirections, in the order they appeared in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
    pub span: Span,
}

/// A `NAME=value` prefix on a command, or the whole command when no
/// words follow.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
}

/// A parsed word: the same part shapes as the token layer, except
/// command substitution is now a fully parsed `Program`.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub parts: Vec<WordPart>,
    pub span: Span,
}

/// A single fragment of a `Word`, mirroring `token::WordPart` with
/// `CmdSubst` fully parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum WordPart {
    Text(String),
    /// One backslash-escaped character, kept distinct from `Text` so
    /// expansion knows not to split or glob on it. See
    /// `token::WordPart::Escaped`.
    Escaped(char),
    SingleQuoted(String),
    DoubleQuoted(Vec<WordPart>),
    Var(String),
    /// `$?`: the exit code of the last completed command. See
    /// `token::WordPart::LastExit`.
    LastExit,
    CmdSubst(Box<Program>),
}

/// A redirection attached to a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Redirect {
    Out {
        stderr: bool,
        append: bool,
        target: Word,
    },
    In {
        target: Word,
    },
    StderrToStdout,
}
