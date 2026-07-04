//! Expression evaluation with SQL three-valued (`NULL`) logic.

use sql::ast::{BinOp, Expr, UnOp, Value};

use crate::catalog::TableSchema;
use crate::EngineError;

/// Evaluate `expr`, resolving leaf nodes (`Column` / `Aggregate`) through `leaf`.
///
/// `leaf` returns `Some(result)` for a node it resolves and `None` for nodes the
/// generic walker should handle (literals, operators). This lets the same 3-valued
/// logic serve single-table rows, joined rows with qualified columns, and
/// post-aggregation rows where an aggregate reads a precomputed column.
pub fn eval_with<F>(expr: &Expr, leaf: &F) -> Result<Value, EngineError>
where
    F: Fn(&Expr) -> Option<Result<Value, EngineError>>,
{
    if let Some(v) = leaf(expr) {
        return v;
    }
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Unary { op, expr } => {
            let v = eval_with(expr, leaf)?;
            apply_unary(*op, v)
        }
        Expr::IsNull { expr, negated } => {
            let v = eval_with(expr, leaf)?;
            let is_null = matches!(v, Value::Null);
            Ok(Value::Boolean(if *negated { !is_null } else { is_null }))
        }
        Expr::Binary { op, left, right } => {
            let l = eval_with(left, leaf)?;
            let r = eval_with(right, leaf)?;
            eval_binary(*op, l, r)
        }
        Expr::Column { table, name } => Err(EngineError::UnknownColumn(match table {
            Some(t) => format!("{t}.{name}"),
            None => name.clone(),
        })),
        Expr::Aggregate { .. } => Err(EngineError::Unsupported(
            "aggregate function not allowed here".into(),
        )),
    }
}

/// Evaluate `expr` against a single-table `row` (column order matches `schema`).
/// For contexts without a row (e.g. `INSERT ... VALUES`), pass an empty schema/row.
pub fn eval(expr: &Expr, schema: &TableSchema, row: &[Value]) -> Result<Value, EngineError> {
    eval_with(expr, &|e: &Expr| match e {
        Expr::Column { name, .. } => Some(
            schema
                .column_index(name)
                .map(|i| row[i].clone())
                .ok_or_else(|| EngineError::UnknownColumn(name.clone())),
        ),
        _ => None,
    })
}

fn apply_unary(op: UnOp, v: Value) -> Result<Value, EngineError> {
    match (op, v) {
        (_, Value::Null) => Ok(Value::Null),
        (UnOp::Neg, Value::Integer(x)) => Ok(Value::Integer(-x)),
        (UnOp::Neg, Value::Real(x)) => Ok(Value::Real(-x)),
        (UnOp::Not, Value::Boolean(b)) => Ok(Value::Boolean(!b)),
        (op, v) => Err(EngineError::Type(format!("cannot apply {op:?} to {v:?}"))),
    }
}

fn eval_binary(op: BinOp, l: Value, r: Value) -> Result<Value, EngineError> {
    use BinOp::*;
    // logical connectives implement three-valued logic before NULL short-circuits
    match op {
        And => return Ok(and3(as_bool(&l)?, as_bool(&r)?)),
        Or => return Ok(or3(as_bool(&l)?, as_bool(&r)?)),
        _ => {}
    }
    if matches!(l, Value::Null) || matches!(r, Value::Null) {
        return Ok(Value::Null);
    }
    match op {
        Eq | Ne | Lt | Le | Gt | Ge => cmp(op, &l, &r),
        Add | Sub | Mul | Div => arith(op, &l, &r),
        And | Or => unreachable!(),
    }
}

/// `NULL` maps to `None`; a non-boolean is a type error.
fn as_bool(v: &Value) -> Result<Option<bool>, EngineError> {
    match v {
        Value::Null => Ok(None),
        Value::Boolean(b) => Ok(Some(*b)),
        other => Err(EngineError::Type(format!(
            "expected BOOLEAN, got {other:?}"
        ))),
    }
}

fn and3(a: Option<bool>, b: Option<bool>) -> Value {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Value::Boolean(false),
        (Some(true), Some(true)) => Value::Boolean(true),
        _ => Value::Null,
    }
}
fn or3(a: Option<bool>, b: Option<bool>) -> Value {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Value::Boolean(true),
        (Some(false), Some(false)) => Value::Boolean(false),
        _ => Value::Null,
    }
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Integer(x) => Some(*x as f64),
        Value::Real(x) => Some(*x),
        _ => None,
    }
}

fn cmp(op: BinOp, l: &Value, r: &Value) -> Result<Value, EngineError> {
    let ord = match (l, r) {
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        _ => match (num(l), num(r)) {
            (Some(a), Some(b)) => a
                .partial_cmp(&b)
                .ok_or_else(|| EngineError::Type("uncomparable (NaN) values".into()))?,
            _ => return Err(EngineError::Type(format!("cannot compare {l:?} and {r:?}"))),
        },
    };
    use std::cmp::Ordering::*;
    let result = match op {
        BinOp::Eq => ord == Equal,
        BinOp::Ne => ord != Equal,
        BinOp::Lt => ord == Less,
        BinOp::Le => ord != Greater,
        BinOp::Gt => ord == Greater,
        BinOp::Ge => ord != Less,
        _ => unreachable!(),
    };
    Ok(Value::Boolean(result))
}

fn arith(op: BinOp, l: &Value, r: &Value) -> Result<Value, EngineError> {
    // integer arithmetic stays integer; any real promotes to real
    if let (Value::Integer(a), Value::Integer(b)) = (l, r) {
        let v = match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => {
                if *b == 0 {
                    return Err(EngineError::Type("division by zero".into()));
                }
                a / b
            }
            _ => unreachable!(),
        };
        return Ok(Value::Integer(v));
    }
    let (a, b) = (
        num(l).ok_or_else(|| EngineError::Type(format!("non-numeric operand {l:?}")))?,
        num(r).ok_or_else(|| EngineError::Type(format!("non-numeric operand {r:?}")))?,
    );
    let v = match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => {
            if b == 0.0 {
                return Err(EngineError::Type("division by zero".into()));
            }
            a / b
        }
        _ => unreachable!(),
    };
    Ok(Value::Real(v))
}
