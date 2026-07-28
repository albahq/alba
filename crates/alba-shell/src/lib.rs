//! Alba's embedded shell: a deterministic, cross-platform, POSIX-like
//! interpreter for beam commands. Standalone by design: this crate
//! depends on no other Alba crate.

mod error;
// The lexer and token model are exercised by their own inline tests for
// now; the parser lands in the next step and starts calling `lex`/`lex_at`
// from production code, at which point this allowance goes away.
#[allow(dead_code)]
mod lexer;
#[allow(dead_code)]
mod token;

pub use error::ShellParseError;
pub use token::Span;
