//! Alba's embedded shell: a deterministic, cross-platform, POSIX-like
//! interpreter for beam commands. Standalone by design: this crate
//! depends on no other Alba crate.

mod ast;
mod error;
mod lexer;
mod parser;
mod token;

pub use ast::Program;
pub use error::ShellParseError;
pub use parser::parse;
pub use token::Span;
