//! [`Expr`], [`BinOp`], and the precedence-climbing expression parser.
//!
//! The parser methods here are inherent methods on
//! [`crate::parser::Parser`] (an `impl` block for that type, physically
//! located in this file rather than `parser.rs`): they share its token
//! stream and cursor rather than duplicating that machinery. Only
//! [`Parser::parse_expr`] is called from outside this module (by
//! `parser.rs`'s `let` bindings and by `template.rs`'s interpolation
//! bodies); the rest of the chain is private to keep the precedence levels
//! an implementation detail.
//!
//! Precedence, low to high: `||`, `&&`, `==`/`!=`, `+`, atoms. `if cond
//! then a else b` is parsed as an atom whose three parts each recurse back
//! into the full expression grammar, so e.g. `else "no" + suffix` parses
//! the whole `+` expression as the `else` branch.

use crate::ast::Spanned;
use crate::parser::{ParseError, Parser};
use crate::template::{self, StringTemplate};
use crate::token::TokenKind;

/// An expression, as used in `let` bindings and interpolated `{expr}`
/// segments of a [`StringTemplate`].
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Str(StringTemplate),
    Bool(bool),
    Var(Spanned<String>),
    Call {
        name: Spanned<String>,
        args: Vec<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    /// `object.field`, member access. Only the `git` object has fields;
    /// which ones is `alba-core`'s concern, not the grammar's.
    Field {
        object: Spanned<String>,
        field: Spanned<String>,
    },
}

/// A binary operator usable in an [`Expr::Binary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    NotEq,
    And,
    Or,
    /// `+`, string concatenation.
    Concat,
}

impl<'a> Parser<'a> {
    /// Parses a full expression: the entry point for `let` bindings,
    /// function-call arguments, and interpolation bodies.
    pub(crate) fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_or()
    }

    /// `||`, left-associative, lowest precedence.
    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while self.check(&TokenKind::OrOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `&&`, left-associative, binds tighter than `||`.
    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_equality()?;
        while self.check(&TokenKind::AndAnd) {
            self.advance();
            let rhs = self.parse_equality()?;
            lhs = Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `==`/`!=`, left-associative, binds tighter than `&&`.
    fn parse_equality(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = if self.check(&TokenKind::EqEq) {
                BinOp::Eq
            } else if self.check(&TokenKind::NotEq) {
                BinOp::NotEq
            } else {
                break;
            };
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `+` (string concatenation), left-associative, binds tighter than
    /// `==`/`!=`.
    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_atom()?;
        while self.check(&TokenKind::Plus) {
            self.advance();
            let rhs = self.parse_atom()?;
            lhs = Expr::Binary {
                op: BinOp::Concat,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// A single atom: a string, boolean, variable, call, or `if/then/else`
    /// expression. The highest-precedence level.
    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        match self.peek().kind.clone() {
            TokenKind::KwIf => self.parse_if(),
            TokenKind::Str(_) => {
                let tok = self.advance();
                let template = template::parse_template_at(self.source(), tok.span)?;
                Ok(Expr::Str(template))
            }
            TokenKind::KwTrue => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            TokenKind::KwFalse => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            TokenKind::Ident(name) => {
                let name_span = self.advance().span;
                if self.check(&TokenKind::Dot) {
                    self.advance();
                    let field = self.eat_ident()?;
                    return Ok(Expr::Field {
                        object: Spanned::new(name, name_span),
                        field,
                    });
                }
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if self.check(&TokenKind::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen, "`)`")?;
                    Ok(Expr::Call {
                        name: Spanned::new(name, name_span),
                        args,
                    })
                } else {
                    Ok(Expr::Var(Spanned::new(name, name_span)))
                }
            }
            _ => Err(self.unexpected("an expression")),
        }
    }

    /// `if cond then a else b`. Each part recurses into the full
    /// expression grammar.
    fn parse_if(&mut self) -> Result<Expr, ParseError> {
        self.expect(TokenKind::KwIf, "`if`")?;
        let cond = self.parse_expr()?;
        self.expect(TokenKind::KwThen, "`then`")?;
        let then = self.parse_expr()?;
        self.expect(TokenKind::KwElse, "`else`")?;
        let otherwise = self.parse_expr()?;
        Ok(Expr::If {
            cond: Box::new(cond),
            then: Box::new(then),
            otherwise: Box::new(otherwise),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::tokenize;

    /// Parses `source` as a single standalone expression (no surrounding
    /// beam/let syntax), asserting it consumes the whole input.
    fn parse_expr_str(source: &str) -> Expr {
        let tokens = tokenize(source).expect("tokenize failed");
        let mut parser = Parser::new(tokens, source);
        let expr = parser.parse_expr().expect("parse failed");
        assert!(parser.is_eof(), "trailing tokens after expression");
        expr
    }

    #[test]
    fn parses_if_then_else_and_precedence() {
        // && binds tighter than ||, == tighter than &&, + tighter than ==
        let e = parse_expr_str(r#"if a == "x" && b then "yes" else "no" + suffix"#);
        insta::assert_debug_snapshot!(e);
    }
}
