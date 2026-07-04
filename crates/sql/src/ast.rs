//! Abstract syntax tree + the value/type model shared across the SQL layer.

/// A column's declared type.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataType {
    Integer,
    Real,
    Text,
    Boolean,
}

/// A runtime SQL value, including the three-valued `NULL`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Boolean(bool),
}

/// Binary operators, in the grammar's precedence tiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
}

/// Unary operators.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnOp {
    Not,
    Neg,
}

/// An aggregate function.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// A scalar expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A column reference, optionally qualified by a table name/alias.
    Column {
        table: Option<String>,
        name: String,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    /// An aggregate call; `arg == None` is `COUNT(*)`.
    Aggregate {
        func: AggFunc,
        arg: Option<Box<Expr>>,
    },
}

impl Expr {
    /// Convenience constructor for an unqualified column reference.
    pub fn col(name: impl Into<String>) -> Expr {
        Expr::Column {
            table: None,
            name: name.into(),
        }
    }
}

/// A column definition inside `CREATE TABLE`.
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub not_null: bool,
    pub primary_key: bool,
}

/// A `SELECT` projection item.
#[derive(Clone, Debug, PartialEq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(String),
    /// `<expr> [AS alias]`
    Expr { expr: Expr, alias: Option<String> },
}

/// A base table reference with an optional alias.
#[derive(Clone, Debug, PartialEq)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
}

impl TableRef {
    /// The name this reference is addressed by (alias if present, else table name).
    pub fn key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// A join kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinType {
    Inner,
    Left,
}

/// One `JOIN <right> ON <predicate>` clause in a left-deep chain.
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    pub join_type: JoinType,
    pub right: TableRef,
    pub on: Expr,
}

/// `ORDER BY <expr> [ASC|DESC]`.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub descending: bool,
}

/// A `SELECT` query.
#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    pub items: Vec<SelectItem>,
    pub from: TableRef,
    pub joins: Vec<Join>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

/// A top-level SQL statement.
#[derive(Clone, Debug, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<ColumnDef>,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select(Select),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    /// `EXPLAIN <select>` — return the physical plan instead of running it.
    Explain(Box<Statement>),
}
