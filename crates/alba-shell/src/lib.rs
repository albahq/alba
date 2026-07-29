//! Alba's embedded shell: a deterministic, cross-platform, POSIX-like
//! interpreter for beam commands. Standalone by design: this crate
//! depends on no other Alba crate.

mod ast;
mod builtins;
mod error;
mod expand;
mod interp;
mod io;
mod lexer;
mod parser;
mod spawn;
mod state;
mod token;

pub use ast::Program;
pub use error::ShellParseError;
pub use interp::{ShellEnv, ShellOutputLine, ShellResult, ShellStream, execute};
pub use parser::parse;
pub use token::Span;
