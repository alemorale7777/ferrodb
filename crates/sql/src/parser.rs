//! Recursive-descent statement parser with a Pratt expression parser.
//!
//! Precedence (low → high): `OR` < `AND` < `NOT` < comparisons/`IS NULL`
//! < `+ -` < `* /` < unary minus.

use crate::ast::*;
use crate::token::{Keyword, Token};
use crate::SqlError;

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, SqlError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }
    fn next(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, want: &Token) -> PResult<()> {
        if self.peek() == want {
            self.next();
            Ok(())
        } else {
            Err(SqlError::Parse(format!(
                "expected {want:?}, found {:?}",
                self.peek()
            )))
        }
    }
    fn eat_kw(&mut self, kw: Keyword) -> PResult<()> {
        if matches!(self.peek(), Token::Keyword(k) if *k == kw) {
            self.next();
            Ok(())
        } else {
            Err(SqlError::Parse(format!(
                "expected {kw:?}, found {:?}",
                self.peek()
            )))
        }
    }
    fn is_kw(&self, kw: Keyword) -> bool {
        matches!(self.peek(), Token::Keyword(k) if *k == kw)
    }
    fn ident(&mut self) -> PResult<String> {
        match self.next() {
            Token::Ident(s) => Ok(s),
            other => Err(SqlError::Parse(format!(
                "expected identifier, found {other:?}"
            ))),
        }
    }

    // ---- statements -------------------------------------------------------

    pub fn parse_statement(&mut self) -> PResult<Statement> {
        let stmt = match self.peek() {
            Token::Keyword(Keyword::Create) => self.parse_create()?,
            Token::Keyword(Keyword::Drop) => self.parse_drop()?,
            Token::Keyword(Keyword::Insert) => self.parse_insert()?,
            Token::Keyword(Keyword::Select) => self.parse_select()?,
            Token::Keyword(Keyword::Update) => self.parse_update()?,
            Token::Keyword(Keyword::Delete) => self.parse_delete()?,
            other => {
                return Err(SqlError::Parse(format!(
                    "unexpected start of statement: {other:?}"
                )))
            }
        };
        // optional trailing semicolon, then EOF
        if self.peek() == &Token::Semicolon {
            self.next();
        }
        match self.peek() {
            Token::Eof => Ok(stmt),
            other => Err(SqlError::Parse(format!(
                "trailing tokens after statement: {other:?}"
            ))),
        }
    }

    fn parse_type(&mut self) -> PResult<DataType> {
        match self.next() {
            Token::Keyword(Keyword::Integer) => Ok(DataType::Integer),
            Token::Keyword(Keyword::Real) => Ok(DataType::Real),
            Token::Keyword(Keyword::Text) => Ok(DataType::Text),
            Token::Keyword(Keyword::Boolean) => Ok(DataType::Boolean),
            other => Err(SqlError::Parse(format!("expected a type, found {other:?}"))),
        }
    }

    fn parse_create(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Create)?;
        self.eat_kw(Keyword::Table)?;
        let name = self.ident()?;
        self.eat(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            let col_name = self.ident()?;
            let data_type = self.parse_type()?;
            let mut not_null = false;
            let mut primary_key = false;
            loop {
                if self.is_kw(Keyword::Not) {
                    self.next();
                    self.eat_kw(Keyword::Null)?;
                    not_null = true;
                } else if self.is_kw(Keyword::Primary) {
                    self.next();
                    self.eat_kw(Keyword::Key)?;
                    primary_key = true;
                    not_null = true; // a PK is implicitly NOT NULL
                } else {
                    break;
                }
            }
            columns.push(ColumnDef {
                name: col_name,
                data_type,
                not_null,
                primary_key,
            });
            if self.peek() == &Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        self.eat(&Token::RParen)?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_drop(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Drop)?;
        self.eat_kw(Keyword::Table)?;
        let name = self.ident()?;
        Ok(Statement::DropTable { name })
    }

    fn parse_insert(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Insert)?;
        self.eat_kw(Keyword::Into)?;
        let table = self.ident()?;
        let columns = if self.peek() == &Token::LParen {
            self.next();
            let mut cols = Vec::new();
            loop {
                cols.push(self.ident()?);
                if self.peek() == &Token::Comma {
                    self.next();
                } else {
                    break;
                }
            }
            self.eat(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        self.eat_kw(Keyword::Values)?;
        let mut rows = Vec::new();
        loop {
            self.eat(&Token::LParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_expr(0)?);
                if self.peek() == &Token::Comma {
                    self.next();
                } else {
                    break;
                }
            }
            self.eat(&Token::RParen)?;
            rows.push(row);
            if self.peek() == &Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn parse_select(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Select)?;
        let mut items = Vec::new();
        loop {
            if self.peek() == &Token::Star {
                self.next();
                items.push(SelectItem::Wildcard);
            } else {
                items.push(SelectItem::Column(self.ident()?));
            }
            if self.peek() == &Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        self.eat_kw(Keyword::From)?;
        let from = self.ident()?;
        let filter = if self.is_kw(Keyword::Where) {
            self.next();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let order_by = if self.is_kw(Keyword::Order) {
            self.next();
            self.eat_kw(Keyword::By)?;
            let column = self.ident()?;
            let descending = if self.is_kw(Keyword::Desc) {
                self.next();
                true
            } else {
                if self.is_kw(Keyword::Asc) {
                    self.next();
                }
                false
            };
            Some(OrderBy { column, descending })
        } else {
            None
        };
        let mut limit = None;
        let mut offset = None;
        if self.is_kw(Keyword::Limit) {
            self.next();
            limit = Some(self.integer()?);
            if self.is_kw(Keyword::Offset) {
                self.next();
                offset = Some(self.integer()?);
            }
        }
        Ok(Statement::Select {
            items,
            from,
            filter,
            order_by,
            limit,
            offset,
        })
    }

    fn parse_update(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Update)?;
        let table = self.ident()?;
        self.eat_kw(Keyword::Set)?;
        let mut assignments = Vec::new();
        loop {
            let col = self.ident()?;
            self.eat(&Token::Eq)?;
            let val = self.parse_expr(0)?;
            assignments.push((col, val));
            if self.peek() == &Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        let filter = if self.is_kw(Keyword::Where) {
            self.next();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Update {
            table,
            assignments,
            filter,
        })
    }

    fn parse_delete(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Delete)?;
        self.eat_kw(Keyword::From)?;
        let table = self.ident()?;
        let filter = if self.is_kw(Keyword::Where) {
            self.next();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        Ok(Statement::Delete { table, filter })
    }

    fn integer(&mut self) -> PResult<u64> {
        match self.next() {
            Token::Int(n) if n >= 0 => Ok(n as u64),
            other => Err(SqlError::Parse(format!(
                "expected a non-negative integer, found {other:?}"
            ))),
        }
    }

    // ---- expressions (Pratt) ---------------------------------------------

    fn parse_expr(&mut self, min_bp: u8) -> PResult<Expr> {
        let mut lhs = self.parse_prefix()?;
        loop {
            // IS [NOT] NULL postfix (comparison precedence)
            if self.is_kw(Keyword::Is) {
                let is_bp = 5;
                if is_bp <= min_bp {
                    break;
                }
                self.next();
                let negated = if self.is_kw(Keyword::Not) {
                    self.next();
                    true
                } else {
                    false
                };
                self.eat_kw(Keyword::Null)?;
                lhs = Expr::IsNull {
                    expr: Box::new(lhs),
                    negated,
                };
                continue;
            }
            let (op, l_bp, r_bp) = match self.infix_op() {
                Some(x) => x,
                None => break,
            };
            if l_bp <= min_bp {
                break;
            }
            self.next();
            let rhs = self.parse_expr(r_bp)?;
            lhs = Expr::Binary {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn infix_op(&self) -> Option<(BinOp, u8, u8)> {
        Some(match self.peek() {
            Token::Keyword(Keyword::Or) => (BinOp::Or, 1, 2),
            Token::Keyword(Keyword::And) => (BinOp::And, 3, 4),
            Token::Eq => (BinOp::Eq, 5, 6),
            Token::Ne => (BinOp::Ne, 5, 6),
            Token::Lt => (BinOp::Lt, 5, 6),
            Token::Le => (BinOp::Le, 5, 6),
            Token::Gt => (BinOp::Gt, 5, 6),
            Token::Ge => (BinOp::Ge, 5, 6),
            Token::Plus => (BinOp::Add, 7, 8),
            Token::Minus => (BinOp::Sub, 7, 8),
            Token::Star => (BinOp::Mul, 9, 10),
            Token::Slash => (BinOp::Div, 9, 10),
            _ => return None,
        })
    }

    fn parse_prefix(&mut self) -> PResult<Expr> {
        match self.next() {
            Token::Int(n) => Ok(Expr::Literal(Value::Integer(n))),
            Token::Float(f) => Ok(Expr::Literal(Value::Real(f))),
            Token::Str(s) => Ok(Expr::Literal(Value::Text(s))),
            Token::Keyword(Keyword::True) => Ok(Expr::Literal(Value::Boolean(true))),
            Token::Keyword(Keyword::False) => Ok(Expr::Literal(Value::Boolean(false))),
            Token::Keyword(Keyword::Null) => Ok(Expr::Literal(Value::Null)),
            Token::Ident(name) => Ok(Expr::Column(name)),
            Token::LParen => {
                let e = self.parse_expr(0)?;
                self.eat(&Token::RParen)?;
                Ok(e)
            }
            Token::Minus => {
                let e = self.parse_expr(10)?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(e),
                })
            }
            Token::Keyword(Keyword::Not) => {
                let e = self.parse_expr(4)?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                })
            }
            other => Err(SqlError::Parse(format!(
                "unexpected token in expression: {other:?}"
            ))),
        }
    }
}
