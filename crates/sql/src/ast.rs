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

/// A scalar expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(String),
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
    Wildcard,
    Column(String),
}

/// `ORDER BY <column> [ASC|DESC]`.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
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
    Select {
        items: Vec<SelectItem>,
        from: String,
        filter: Option<Expr>,
        order_by: Option<OrderBy>,
        limit: Option<u64>,
        offset: Option<u64>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
}
