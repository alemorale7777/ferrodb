//! Physical query plans and the row operators that execute them.
//!
//! A [`RowSet`] (a schema of qualified columns plus materialised rows) flows
//! between operators. The [`Plan`] tree is pure data the planner builds and the
//! executor walks; scan nodes are executed by the engine (they need catalog and
//! MVCC access), every other operator is a pure function over `RowSet`s here.

use std::cmp::Ordering;
use std::collections::HashMap;

use sql::ast::{AggFunc, BinOp, DataType, Expr, JoinType, UnOp, Value};

use crate::EngineError;

/// A column in a [`RowSet`]'s schema.
#[derive(Clone, Debug)]
pub struct Col {
    /// Qualifier (table name or alias); `None` for computed/projected columns.
    pub table: Option<String>,
    pub name: String,
    pub ty: DataType,
}

/// A materialised intermediate result: a schema plus its rows.
#[derive(Clone, Debug, Default)]
pub struct RowSet {
    pub schema: Vec<Col>,
    pub rows: Vec<Vec<Value>>,
}

/// Resolve a (possibly qualified) column reference to an index in `schema`.
pub fn resolve_col(schema: &[Col], table: Option<&str>, name: &str) -> Result<usize, EngineError> {
    let mut found = None;
    for (i, c) in schema.iter().enumerate() {
        let name_ok = c.name.eq_ignore_ascii_case(name);
        let table_ok = match (table, &c.table) {
            (None, _) => true,
            (Some(q), Some(t)) => q.eq_ignore_ascii_case(t),
            (Some(_), None) => false,
        };
        if name_ok && table_ok {
            if found.is_some() {
                return Err(EngineError::UnknownColumn(format!(
                    "ambiguous column '{name}'"
                )));
            }
            found = Some(i);
        }
    }
    found.ok_or_else(|| EngineError::UnknownColumn(qualified(table, name)))
}

fn qualified(table: Option<&str>, name: &str) -> String {
    match table {
        Some(t) => format!("{t}.{name}"),
        None => name.to_string(),
    }
}

/// Evaluate `expr` against `row` under `schema` (columns resolved by name/qualifier).
pub fn eval_row(expr: &Expr, schema: &[Col], row: &[Value]) -> Result<Value, EngineError> {
    crate::eval::eval_with(expr, &|e: &Expr| match e {
        Expr::Column { table, name } => {
            Some(resolve_col(schema, table.as_deref(), name).map(|i| row[i].clone()))
        }
        _ => None,
    })
}

fn is_true(v: &Value) -> bool {
    matches!(v, Value::Boolean(true))
}

/// SQL sort order: `NULL` sorts first, then by type-appropriate comparison.
pub fn value_cmp(a: &Value, b: &Value) -> Ordering {
    let num = |v: &Value| match v {
        Value::Integer(x) => Some(*x as f64),
        Value::Real(x) => Some(*x),
        _ => None,
    };
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Boolean(x), Value::Boolean(y)) => x.cmp(y),
        _ => match (num(a), num(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => Ordering::Equal,
        },
    }
}

/// The declared type that best fits a value (`NULL` defaults to `Integer`).
pub fn value_type(v: &Value) -> DataType {
    match v {
        Value::Integer(_) => DataType::Integer,
        Value::Real(_) => DataType::Real,
        Value::Text(_) => DataType::Text,
        Value::Boolean(_) => DataType::Boolean,
        Value::Vector(v) => DataType::Vector(v.len() as u16),
        Value::Null => DataType::Integer,
    }
}

/// A hashable encoding of a join/group key value; `None` for `NULL` (never matches).
fn hash_key(v: &Value) -> Option<Vec<u8>> {
    Some(match v {
        Value::Null => return None,
        Value::Integer(x) => {
            let mut k = vec![0u8];
            k.extend_from_slice(&x.to_le_bytes());
            k
        }
        Value::Real(x) => {
            let mut k = vec![1u8];
            k.extend_from_slice(&x.to_bits().to_le_bytes());
            k
        }
        Value::Text(s) => {
            let mut k = vec![2u8];
            k.extend_from_slice(s.as_bytes());
            k
        }
        Value::Boolean(b) => vec![3u8, *b as u8],
        Value::Vector(v) => {
            // Vectors can be grouped/joined on exact bit equality (the only
            // sane equality for floats used as keys).
            let mut k = vec![4u8];
            for x in v {
                k.extend_from_slice(&x.to_bits().to_le_bytes());
            }
            k
        }
    })
}

fn combine(left: &[Value], right: &[Value]) -> Vec<Value> {
    left.iter().chain(right).cloned().collect()
}

// ---- join operators -------------------------------------------------------

/// Nested-loop join: applies `on` to every pair. `LEFT` null-pads unmatched left rows.
pub fn nested_loop_join(
    left: &RowSet,
    right: &RowSet,
    jt: JoinType,
    on: &Expr,
) -> Result<RowSet, EngineError> {
    let mut schema = left.schema.clone();
    schema.extend(right.schema.iter().cloned());
    let mut rows = Vec::new();
    for lrow in &left.rows {
        let mut matched = false;
        for rrow in &right.rows {
            let row = combine(lrow, rrow);
            if is_true(&eval_row(on, &schema, &row)?) {
                rows.push(row);
                matched = true;
            }
        }
        if !matched && jt == JoinType::Left {
            let mut row = lrow.clone();
            row.extend(std::iter::repeat_n(Value::Null, right.schema.len()));
            rows.push(row);
        }
    }
    Ok(RowSet { schema, rows })
}

/// Hash join on `left_key = right_key`, re-checking the full `on` predicate per
/// candidate pair (so extra `ON` conditions and `LEFT` semantics are honoured).
pub fn hash_join(
    left: &RowSet,
    right: &RowSet,
    jt: JoinType,
    left_key: &Expr,
    right_key: &Expr,
    on: &Expr,
) -> Result<RowSet, EngineError> {
    let mut schema = left.schema.clone();
    schema.extend(right.schema.iter().cloned());

    let mut buckets: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
    for (i, rrow) in right.rows.iter().enumerate() {
        if let Some(k) = hash_key(&eval_row(right_key, &right.schema, rrow)?) {
            buckets.entry(k).or_default().push(i);
        }
    }

    let mut rows = Vec::new();
    for lrow in &left.rows {
        let mut matched = false;
        if let Some(k) = hash_key(&eval_row(left_key, &left.schema, lrow)?) {
            if let Some(cands) = buckets.get(&k) {
                for &j in cands {
                    let row = combine(lrow, &right.rows[j]);
                    if is_true(&eval_row(on, &schema, &row)?) {
                        rows.push(row);
                        matched = true;
                    }
                }
            }
        }
        if !matched && jt == JoinType::Left {
            let mut row = lrow.clone();
            row.extend(std::iter::repeat_n(Value::Null, right.schema.len()));
            rows.push(row);
        }
    }
    Ok(RowSet { schema, rows })
}

// ---- filter / sort / project / limit --------------------------------------

pub fn filter_rows(rs: RowSet, pred: &Expr) -> Result<RowSet, EngineError> {
    let RowSet { schema, rows } = rs;
    let mut kept = Vec::new();
    for row in rows {
        if is_true(&eval_row(pred, &schema, &row)?) {
            kept.push(row);
        }
    }
    Ok(RowSet { schema, rows: kept })
}

pub fn sort_rows(rs: RowSet, keys: &[(Expr, bool)]) -> Result<RowSet, EngineError> {
    let RowSet { schema, rows } = rs;
    let mut keyed: Vec<(Vec<Value>, Vec<Value>)> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut kv = Vec::with_capacity(keys.len());
        for (e, _) in keys {
            kv.push(eval_row(e, &schema, &row)?);
        }
        keyed.push((kv, row));
    }
    keyed.sort_by(|a, b| {
        for (i, (_, desc)) in keys.iter().enumerate() {
            let ord = value_cmp(&a.0[i], &b.0[i]);
            let ord = if *desc { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    Ok(RowSet {
        schema,
        rows: keyed.into_iter().map(|(_, r)| r).collect(),
    })
}

pub fn project_rows(rs: RowSet, exprs: &[Expr], names: &[String]) -> Result<RowSet, EngineError> {
    let mut rows = Vec::with_capacity(rs.rows.len());
    for row in &rs.rows {
        let mut out = Vec::with_capacity(exprs.len());
        for e in exprs {
            out.push(eval_row(e, &rs.schema, row)?);
        }
        rows.push(out);
    }
    let schema = names
        .iter()
        .enumerate()
        .map(|(i, n)| Col {
            table: None,
            name: n.clone(),
            ty: rows
                .first()
                .map(|r| value_type(&r[i]))
                .unwrap_or(DataType::Integer),
        })
        .collect();
    Ok(RowSet { schema, rows })
}

pub fn limit_rows(mut rs: RowSet, limit: Option<u64>, offset: Option<u64>) -> RowSet {
    if let Some(off) = offset {
        let off = off as usize;
        rs.rows = if off >= rs.rows.len() {
            Vec::new()
        } else {
            rs.rows.split_off(off)
        };
    }
    if let Some(lim) = limit {
        rs.rows.truncate(lim as usize);
    }
    rs
}

// ---- aggregation ----------------------------------------------------------

/// Group `rs` by `group_by` and fold `aggs` (each an `Expr::Aggregate`). Output
/// columns are the group keys followed by the aggregates, named by `out_names`.
pub fn aggregate_rows(
    rs: RowSet,
    group_by: &[Expr],
    aggs: &[Expr],
    out_names: &[String],
) -> Result<RowSet, EngineError> {
    let mut order: Vec<Vec<u8>> = Vec::new();
    let mut groups: HashMap<Vec<u8>, (Vec<Value>, Vec<usize>)> = HashMap::new();
    for (i, row) in rs.rows.iter().enumerate() {
        let mut keyvals = Vec::with_capacity(group_by.len());
        let mut enc = Vec::new();
        for g in group_by {
            let v = eval_row(g, &rs.schema, row)?;
            enc.extend(hash_key(&v).unwrap_or_else(|| vec![0xFF]));
            enc.push(0xFE);
            keyvals.push(v);
        }
        match groups.entry(enc.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().1.push(i),
            std::collections::hash_map::Entry::Vacant(e) => {
                order.push(enc);
                e.insert((keyvals, vec![i]));
            }
        }
    }
    // A global aggregate over an empty input still yields one row.
    if rs.rows.is_empty() && group_by.is_empty() {
        order.push(Vec::new());
        groups.insert(Vec::new(), (Vec::new(), Vec::new()));
    }

    let mut out_rows = Vec::with_capacity(order.len());
    for key in &order {
        let (keyvals, idxs) = &groups[key];
        let member: Vec<&Vec<Value>> = idxs.iter().map(|&i| &rs.rows[i]).collect();
        let mut out = keyvals.clone();
        for a in aggs {
            out.push(compute_agg(a, &rs.schema, &member)?);
        }
        out_rows.push(out);
    }

    let schema = out_names
        .iter()
        .enumerate()
        .map(|(i, name)| Col {
            table: None,
            name: name.clone(),
            ty: out_rows
                .first()
                .map(|r| value_type(&r[i]))
                .unwrap_or(DataType::Integer),
        })
        .collect();
    Ok(RowSet {
        schema,
        rows: out_rows,
    })
}

fn compute_agg(agg: &Expr, schema: &[Col], rows: &[&Vec<Value>]) -> Result<Value, EngineError> {
    let Expr::Aggregate { func, arg } = agg else {
        return Err(EngineError::Unsupported("expected an aggregate".into()));
    };
    use AggFunc::*;
    match (func, arg) {
        (Count, None) => Ok(Value::Integer(rows.len() as i64)),
        (Count, Some(e)) => {
            let mut n = 0i64;
            for r in rows {
                if !matches!(eval_row(e, schema, r)?, Value::Null) {
                    n += 1;
                }
            }
            Ok(Value::Integer(n))
        }
        (Sum, Some(e)) | (Avg, Some(e)) => {
            let mut sum = 0f64;
            let mut count = 0i64;
            let mut all_int = true;
            for r in rows {
                match eval_row(e, schema, r)? {
                    Value::Null => {}
                    Value::Integer(x) => {
                        sum += x as f64;
                        count += 1;
                    }
                    Value::Real(x) => {
                        sum += x;
                        count += 1;
                        all_int = false;
                    }
                    other => return Err(EngineError::Type(format!("cannot aggregate {other:?}"))),
                }
            }
            if count == 0 {
                return Ok(Value::Null);
            }
            if matches!(func, Avg) {
                Ok(Value::Real(sum / count as f64))
            } else if all_int {
                Ok(Value::Integer(sum as i64))
            } else {
                Ok(Value::Real(sum))
            }
        }
        (Min, Some(e)) | (Max, Some(e)) => {
            let want_min = matches!(func, Min);
            let mut best: Option<Value> = None;
            for r in rows {
                let v = eval_row(e, schema, r)?;
                if matches!(v, Value::Null) {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let ord = value_cmp(&v, &b);
                        let take = (want_min && ord == Ordering::Less)
                            || (!want_min && ord == Ordering::Greater);
                        if take {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        (_, None) => Err(EngineError::Unsupported(
            "this aggregate requires an argument".into(),
        )),
    }
}

// ---- the physical plan tree -----------------------------------------------

/// How a base table is accessed.
#[derive(Clone, Debug)]
pub enum Access {
    /// Full sequential scan of all visible rows.
    Seq,
    /// B+-tree seek on the primary key: `pk <op> key`.
    IndexSeek { op: BinOp, key: Expr },
    /// B+-tree range scan on the primary key. `lo` is an inclusive lower bound
    /// and `hi` an exclusive upper bound (either may be absent). The scan's
    /// residual `filter` still applies, so these bounds only prune the range —
    /// the exact predicate semantics come from the filter.
    IndexRange { lo: Option<Expr>, hi: Option<Expr> },
    /// HNSW approximate top-k scan (M9): fetch the ~k nearest rows to `query`
    /// by `column`, resolved through the primary B+-tree. Approximate — the
    /// plan's Sort/Limit above re-order and truncate the candidates exactly,
    /// and the scan's residual filter (if any) is applied predicate-aware
    /// inside the traversal (see `Database::exec_scan`).
    VectorTopK {
        column: String,
        query: Expr,
        k: usize,
    },
}

#[derive(Clone, Debug)]
pub enum Plan {
    Scan {
        table: String,
        alias: String,
        access: Access,
        filter: Option<Expr>,
        est: f64,
    },
    Join {
        left: Box<Plan>,
        right: Box<Plan>,
        jt: JoinType,
        on: Expr,
        /// `Some((left_key, right_key))` selects a hash join; `None` is nested loop.
        hash_keys: Option<(Expr, Expr)>,
        est: f64,
    },
    Filter {
        input: Box<Plan>,
        pred: Expr,
        est: f64,
    },
    Aggregate {
        input: Box<Plan>,
        group_by: Vec<Expr>,
        aggs: Vec<Expr>,
        out_names: Vec<String>,
        est: f64,
    },
    Sort {
        input: Box<Plan>,
        keys: Vec<(Expr, bool)>,
        est: f64,
    },
    Project {
        input: Box<Plan>,
        exprs: Vec<Expr>,
        names: Vec<String>,
        est: f64,
    },
    Limit {
        input: Box<Plan>,
        limit: Option<u64>,
        offset: Option<u64>,
        est: f64,
    },
}

impl Plan {
    pub fn est(&self) -> f64 {
        match self {
            Plan::Scan { est, .. }
            | Plan::Join { est, .. }
            | Plan::Filter { est, .. }
            | Plan::Aggregate { est, .. }
            | Plan::Sort { est, .. }
            | Plan::Project { est, .. }
            | Plan::Limit { est, .. } => *est,
        }
    }

    /// Render the plan as indented `EXPLAIN` lines, most-parent first.
    pub fn explain(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.explain_into(0, &mut out);
        out
    }

    fn explain_into(&self, depth: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(depth);
        let rows = format!("  (rows≈{:.0})", self.est());
        match self {
            Plan::Scan {
                table,
                alias,
                access,
                filter,
                ..
            } => {
                let named = if alias == table {
                    table.clone()
                } else {
                    format!("{table} {alias}")
                };
                let line = match access {
                    Access::Seq => format!("SeqScan {named}"),
                    Access::IndexSeek { op, key } => {
                        format!("IndexSeek {named} (pk {} {})", fmt_op(*op), fmt_expr(key))
                    }
                    Access::VectorTopK { column, query, k } => format!(
                        "HnswTopK {named} (distance({column}, {}) LIMIT {k})",
                        fmt_expr(query)
                    ),
                    Access::IndexRange { lo, hi } => {
                        let bound = match (lo, hi) {
                            (Some(l), Some(h)) => {
                                format!("{} <= pk < {}", fmt_expr(l), fmt_expr(h))
                            }
                            (Some(l), None) => format!("pk >= {}", fmt_expr(l)),
                            (None, Some(h)) => format!("pk < {}", fmt_expr(h)),
                            (None, None) => "all".to_string(),
                        };
                        format!("IndexRange {named} ({bound})")
                    }
                };
                let filt = filter
                    .as_ref()
                    .map(|f| format!("  filter: {}", fmt_expr(f)))
                    .unwrap_or_default();
                out.push(format!("{pad}{line}{rows}{filt}"));
            }
            Plan::Join {
                left,
                right,
                jt,
                on,
                hash_keys,
                ..
            } => {
                let kind = match jt {
                    JoinType::Inner => "Inner",
                    JoinType::Left => "Left",
                };
                let algo = if hash_keys.is_some() {
                    "HashJoin"
                } else {
                    "NestedLoopJoin"
                };
                out.push(format!("{pad}{algo} [{kind}] on {}{rows}", fmt_expr(on)));
                left.explain_into(depth + 1, out);
                right.explain_into(depth + 1, out);
            }
            Plan::Filter { input, pred, .. } => {
                out.push(format!("{pad}Filter {}{rows}", fmt_expr(pred)));
                input.explain_into(depth + 1, out);
            }
            Plan::Aggregate {
                input,
                group_by,
                aggs,
                ..
            } => {
                let gb: Vec<String> = group_by.iter().map(fmt_expr).collect();
                let ag: Vec<String> = aggs.iter().map(fmt_expr).collect();
                out.push(format!(
                    "{pad}HashAggregate group=[{}] aggs=[{}]{rows}",
                    gb.join(", "),
                    ag.join(", ")
                ));
                input.explain_into(depth + 1, out);
            }
            Plan::Sort { input, keys, .. } => {
                let ks: Vec<String> = keys
                    .iter()
                    .map(|(e, d)| format!("{}{}", fmt_expr(e), if *d { " DESC" } else { "" }))
                    .collect();
                out.push(format!("{pad}Sort [{}]{rows}", ks.join(", ")));
                input.explain_into(depth + 1, out);
            }
            Plan::Project {
                input,
                exprs,
                names,
                ..
            } => {
                let cols: Vec<String> = exprs
                    .iter()
                    .zip(names)
                    .map(|(e, n)| {
                        let s = fmt_expr(e);
                        if &s == n {
                            s
                        } else {
                            format!("{s} AS {n}")
                        }
                    })
                    .collect();
                out.push(format!("{pad}Project [{}]{rows}", cols.join(", ")));
                input.explain_into(depth + 1, out);
            }
            Plan::Limit {
                input,
                limit,
                offset,
                ..
            } => {
                out.push(format!(
                    "{pad}Limit {}{}{rows}",
                    limit.map(|l| l.to_string()).unwrap_or_else(|| "ALL".into()),
                    offset.map(|o| format!(" offset {o}")).unwrap_or_default()
                ));
                input.explain_into(depth + 1, out);
            }
        }
    }
}

// ---- expression pretty-printing (for EXPLAIN) -----------------------------

fn fmt_op(op: BinOp) -> &'static str {
    match op {
        BinOp::Or => "OR",
        BinOp::And => "AND",
        BinOp::Eq => "=",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
    }
}

fn fmt_val(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Integer(x) => x.to_string(),
        Value::Real(x) => x.to_string(),
        Value::Text(s) => format!("'{s}'"),
        Value::Boolean(b) => b.to_string(),
        Value::Vector(v) => {
            // Keep EXPLAIN lines readable for wide vectors.
            if v.len() > 4 {
                format!("[{}, {}, ... ({} dims)]", v[0], v[1], v.len())
            } else {
                format!("{v:?}")
            }
        }
    }
}

/// Compact single-line rendering of an expression.
pub fn fmt_expr(e: &Expr) -> String {
    match e {
        Expr::Literal(v) => fmt_val(v),
        Expr::Column {
            table: Some(t),
            name,
        } => format!("{t}.{name}"),
        Expr::Column { table: None, name } => name.clone(),
        Expr::Binary { op, left, right } => {
            format!("{} {} {}", fmt_expr(left), fmt_op(*op), fmt_expr(right))
        }
        Expr::Unary { op, expr } => {
            let sym = match op {
                UnOp::Not => "NOT ",
                UnOp::Neg => "-",
            };
            format!("{sym}{}", fmt_expr(expr))
        }
        Expr::IsNull { expr, negated } => {
            format!(
                "{} IS {}NULL",
                fmt_expr(expr),
                if *negated { "NOT " } else { "" }
            )
        }
        Expr::Distance { left, right } => {
            format!("distance({}, {})", fmt_expr(left), fmt_expr(right))
        }
        Expr::Aggregate { func, arg } => {
            let name = match func {
                AggFunc::Count => "COUNT",
                AggFunc::Sum => "SUM",
                AggFunc::Avg => "AVG",
                AggFunc::Min => "MIN",
                AggFunc::Max => "MAX",
            };
            let inner = arg
                .as_ref()
                .map(|a| fmt_expr(a))
                .unwrap_or_else(|| "*".into());
            format!("{name}({inner})")
        }
    }
}
