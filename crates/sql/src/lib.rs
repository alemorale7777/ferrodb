//! ferrodb SQL frontend: hand-written lexer + Pratt/recursive-descent parser.
//!
//! `parse(sql)` turns SQL text into an [`ast::Statement`]. No third-party SQL
//! parser is used — the parser is part of the exercise.

pub mod ast;
pub mod parser;
pub mod token;

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum SqlError {
    #[error("lex error: {0}")]
    Lex(String),
    #[error("parse error: {0}")]
    Parse(String),
}

/// Lex and parse a single SQL statement.
pub fn parse(sql: &str) -> Result<ast::Statement, SqlError> {
    let tokens = token::lex(sql)?;
    parser::Parser::new(tokens).parse_statement()
}
