//! Lexer, parser, AST, and diagnostics for the Beamfile DSL.

mod ast;
mod expr;
mod lexer;
mod parser;
mod template;
mod token;

pub use ast::{BeamDecl, BeamRef, ExecutorDecl, File, Import, LetBinding, NamedString, Spanned};
pub use expr::{BinOp, Expr};
pub use lexer::{LexError, Lexer};
pub use parser::{ParseError, parse};
pub use template::{StringTemplate, TemplatePart, parse_template};
pub use token::{Span, Token, TokenKind};
