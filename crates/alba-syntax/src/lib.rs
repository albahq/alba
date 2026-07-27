//! Lexer, parser, AST, and diagnostics for the Beamfile DSL.

mod lexer;
mod token;

pub use lexer::{LexError, Lexer};
pub use token::{Span, Token, TokenKind};
