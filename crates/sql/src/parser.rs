//! Recursive-descent statement parser with a Pratt expression parser.
//!
//! Precedence (low → high): `OR` < `AND` < `NOT` < comparisons/`IS NULL`
//! < `+ -` < `* /` < unary minus.

use crate::ast::*;
use crate::token::{Keyword, Token};
use crate::SqlError;

/// Recognise an aggregate function name (case-insensitive).
fn agg_from(name: &str) -> Option<AggFunc> {
    Some(match name.to_ascii_uppercase().as_str() {
        "COUNT" => AggFunc::Count,
        "SUM" => AggFunc::Sum,
        "AVG" => AggFunc::Avg,
        "MIN" => AggFunc::Min,
        "MAX" => AggFunc::Max,
        _ => return None,
    })
}

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
            Token::Keyword(Keyword::Explain) => {
                self.next();
                Statement::Explain(Box::new(self.parse_select()?))
            }
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
            Token::Keyword(Keyword::Vector) => {
                self.eat(&Token::LParen)?;
                let dim = match self.next() {
                    Token::Int(n) if (1..=u16::MAX as i64).contains(&n) => n as u16,
                    other => {
                        return Err(SqlError::Parse(format!(
                            "VECTOR dimension must be 1..=65535, found {other:?}"
                        )))
                    }
                };
                self.eat(&Token::RParen)?;
                Ok(DataType::Vector(dim))
            }
            other => Err(SqlError::Parse(format!("expected a type, found {other:?}"))),
        }
    }

    fn parse_create(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Create)?;
        if self.is_kw(Keyword::Index) {
            return self.parse_create_index();
        }
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

    /// `CREATE INDEX <name> ON <table> USING HNSW (<column>)` — mirrors
    /// pgvector's `CREATE INDEX ... USING hnsw (col vector_l2_ops)` shape,
    /// minus the opclass (M9 indexes are L2; operators land with M9-stretch).
    fn parse_create_index(&mut self) -> PResult<Statement> {
        self.eat_kw(Keyword::Index)?;
        let name = self.ident()?;
        self.eat_kw(Keyword::On)?;
        let table = self.ident()?;
        self.eat_kw(Keyword::Using)?;
        self.eat_kw(Keyword::Hnsw)?;
        self.eat(&Token::LParen)?;
        let column = self.ident()?;
        self.eat(&Token::RParen)?;
        Ok(Statement::CreateIndex {
            name,
            table,
            column,
        })
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
            items.push(self.parse_select_item()?);
            if self.peek() == &Token::Comma {
                self.next();
            } else {
                break;
            }
        }
        self.eat_kw(Keyword::From)?;
        let from = self.parse_table_ref()?;
        let joins = self.parse_joins()?;
        let filter = if self.is_kw(Keyword::Where) {
            self.next();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let group_by = if self.is_kw(Keyword::Group) {
            self.next();
            self.eat_kw(Keyword::By)?;
            let mut keys = Vec::new();
            loop {
                keys.push(self.parse_expr(0)?);
                if self.peek() == &Token::Comma {
                    self.next();
                } else {
                    break;
                }
            }
            keys
        } else {
            Vec::new()
        };
        let having = if self.is_kw(Keyword::Having) {
            self.next();
            Some(self.parse_expr(0)?)
        } else {
            None
        };
        let order_by = if self.is_kw(Keyword::Order) {
            self.next();
            self.eat_kw(Keyword::By)?;
            let mut keys = Vec::new();
            loop {
                let expr = self.parse_expr(0)?;
                let descending = if self.is_kw(Keyword::Desc) {
                    self.next();
                    true
                } else {
                    if self.is_kw(Keyword::Asc) {
                        self.next();
                    }
                    false
                };
                keys.push(OrderBy { expr, descending });
                if self.peek() == &Token::Comma {
                    self.next();
                } else {
                    break;
                }
            }
            keys
        } else {
            Vec::new()
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
        Ok(Statement::Select(Select {
            items,
            from,
            joins,
            filter,
            group_by,
            having,
            order_by,
            limit,
            offset,
        }))
    }

    fn parse_select_item(&mut self) -> PResult<SelectItem> {
        if self.peek() == &Token::Star {
            self.next();
            return Ok(SelectItem::Wildcard);
        }
        // `t.*`
        if let Token::Ident(name) = self.peek().clone() {
            if self.tokens.get(self.pos + 1) == Some(&Token::Dot)
                && self.tokens.get(self.pos + 2) == Some(&Token::Star)
            {
                self.next();
                self.next();
                self.next();
                return Ok(SelectItem::QualifiedWildcard(name));
            }
        }
        let expr = self.parse_expr(0)?;
        let alias = self.parse_optional_alias()?;
        Ok(SelectItem::Expr { expr, alias })
    }

    /// `AS ident`, or a bare trailing identifier, used as an alias.
    fn parse_optional_alias(&mut self) -> PResult<Option<String>> {
        if self.is_kw(Keyword::As) {
            self.next();
            Ok(Some(self.ident()?))
        } else if matches!(self.peek(), Token::Ident(_)) {
            Ok(Some(self.ident()?))
        } else {
            Ok(None)
        }
    }

    fn parse_table_ref(&mut self) -> PResult<TableRef> {
        let name = self.ident()?;
        let alias = self.parse_optional_alias()?;
        Ok(TableRef { name, alias })
    }

    fn parse_joins(&mut self) -> PResult<Vec<Join>> {
        let mut joins = Vec::new();
        loop {
            let join_type = if self.is_kw(Keyword::Inner) {
                self.next();
                JoinType::Inner
            } else if self.is_kw(Keyword::Left) {
                self.next();
                if self.is_kw(Keyword::Outer) {
                    self.next();
                }
                JoinType::Left
            } else if self.is_kw(Keyword::Join) {
                JoinType::Inner
            } else {
                break;
            };
            self.eat_kw(Keyword::Join)?;
            let right = self.parse_table_ref()?;
            self.eat_kw(Keyword::On)?;
            let on = self.parse_expr(0)?;
            joins.push(Join {
                join_type,
                right,
                on,
            });
        }
        Ok(joins)
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
            Token::Ident(name) => {
                if self.peek() == &Token::LParen {
                    if name.eq_ignore_ascii_case("distance") {
                        self.next(); // (
                        let left = self.parse_expr(0)?;
                        self.eat(&Token::Comma)?;
                        let right = self.parse_expr(0)?;
                        self.eat(&Token::RParen)?;
                        return Ok(Expr::Distance {
                            left: Box::new(left),
                            right: Box::new(right),
                        });
                    }
                    let Some(func) = agg_from(&name) else {
                        return Err(SqlError::Parse(format!("unknown function '{name}'")));
                    };
                    self.next(); // (
                    let arg = if self.peek() == &Token::Star {
                        self.next();
                        if func != AggFunc::Count {
                            return Err(SqlError::Parse(
                                "'*' is only valid as COUNT(*)".to_string(),
                            ));
                        }
                        None
                    } else {
                        Some(Box::new(self.parse_expr(0)?))
                    };
                    self.eat(&Token::RParen)?;
                    Ok(Expr::Aggregate { func, arg })
                } else if self.peek() == &Token::Dot {
                    self.next(); // .
                    let column = self.ident()?;
                    Ok(Expr::Column {
                        table: Some(name),
                        name: column,
                    })
                } else {
                    Ok(Expr::Column { table: None, name })
                }
            }
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
