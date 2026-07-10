//! Hand-written lexer: SQL text → token stream.

use crate::SqlError;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyword {
    Create,
    Table,
    Drop,
    Insert,
    Into,
    Values,
    Select,
    From,
    Where,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    Update,
    Set,
    Delete,
    And,
    Or,
    Not,
    Null,
    Is,
    True,
    False,
    Primary,
    Key,
    Integer,
    Real,
    Text,
    Boolean,
    As,
    Join,
    Inner,
    Left,
    Outer,
    On,
    Group,
    Having,
    Explain,
    Vector,
    Index,
    Using,
    Hnsw,
}

fn keyword_from(word: &str) -> Option<Keyword> {
    use Keyword::*;
    Some(match word.to_ascii_uppercase().as_str() {
        "CREATE" => Create,
        "TABLE" => Table,
        "DROP" => Drop,
        "INSERT" => Insert,
        "INTO" => Into,
        "VALUES" => Values,
        "SELECT" => Select,
        "FROM" => From,
        "WHERE" => Where,
        "ORDER" => Order,
        "BY" => By,
        "ASC" => Asc,
        "DESC" => Desc,
        "LIMIT" => Limit,
        "OFFSET" => Offset,
        "UPDATE" => Update,
        "SET" => Set,
        "DELETE" => Delete,
        "AND" => And,
        "OR" => Or,
        "NOT" => Not,
        "NULL" => Null,
        "IS" => Is,
        "TRUE" => True,
        "FALSE" => False,
        "PRIMARY" => Primary,
        "KEY" => Key,
        "INTEGER" | "INT" => Integer,
        "REAL" | "FLOAT" | "DOUBLE" => Real,
        "TEXT" | "VARCHAR" => Text,
        "BOOLEAN" | "BOOL" => Boolean,
        "AS" => As,
        "JOIN" => Join,
        "INNER" => Inner,
        "LEFT" => Left,
        "OUTER" => Outer,
        "ON" => On,
        "GROUP" => Group,
        "HAVING" => Having,
        "EXPLAIN" => Explain,
        "VECTOR" => Vector,
        "INDEX" => Index,
        "USING" => Using,
        "HNSW" => Hnsw,
        _ => return None,
    })
}

#[derive(Clone, PartialEq, Debug)]
pub enum Token {
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    Keyword(Keyword),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Dot,
    Star,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Slash,
    Eof,
}

/// Tokenize `src` into a `Vec<Token>` ending in `Token::Eof`.
pub fn lex(src: &str) -> Result<Vec<Token>, SqlError> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            c if c.is_ascii_whitespace() => i += 1,
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            ',' => {
                out.push(Token::Comma);
                i += 1;
            }
            ';' => {
                out.push(Token::Semicolon);
                i += 1;
            }
            '.' => {
                out.push(Token::Dot);
                i += 1;
            }
            '*' => {
                out.push(Token::Star);
                i += 1;
            }
            '+' => {
                out.push(Token::Plus);
                i += 1;
            }
            '-' => {
                out.push(Token::Minus);
                i += 1;
            }
            '/' => {
                out.push(Token::Slash);
                i += 1;
            }
            '=' => {
                out.push(Token::Eq);
                i += 1;
            }
            '<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token::Le);
                    i += 2;
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    out.push(Token::Ne);
                    i += 2;
                } else {
                    out.push(Token::Lt);
                    i += 1;
                }
            }
            '>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token::Ge);
                    i += 2;
                } else {
                    out.push(Token::Gt);
                    i += 1;
                }
            }
            '!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    out.push(Token::Ne);
                    i += 2;
                } else {
                    return Err(SqlError::Lex(format!("unexpected character '{c}'")));
                }
            }
            '\'' => {
                // string literal with '' escaping
                let mut s = String::new();
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return Err(SqlError::Lex("unterminated string literal".into()));
                    }
                    let ch = bytes[i] as char;
                    if ch == '\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(ch);
                        i += 1;
                    }
                }
                out.push(Token::Str(s));
            }
            c if c.is_ascii_digit() => {
                let start = i;
                let mut is_float = false;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    if bytes[i] == b'.' {
                        is_float = true;
                    }
                    i += 1;
                }
                let text = &src[start..i];
                if is_float {
                    out.push(Token::Float(
                        text.parse()
                            .map_err(|_| SqlError::Lex(format!("bad number '{text}'")))?,
                    ));
                } else {
                    out.push(Token::Int(
                        text.parse()
                            .map_err(|_| SqlError::Lex(format!("bad number '{text}'")))?,
                    ));
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len()
                    && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
                {
                    i += 1;
                }
                let word = &src[start..i];
                match keyword_from(word) {
                    Some(kw) => out.push(Token::Keyword(kw)),
                    None => out.push(Token::Ident(word.to_string())),
                }
            }
            _ => return Err(SqlError::Lex(format!("unexpected character '{c}'"))),
        }
    }
    out.push(Token::Eof);
    Ok(out)
}
