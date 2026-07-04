//! Pure planning helpers: expression rewriting, aggregate collection, join-key
//! detection, and output naming. The stateful part of planning (base-table
//! cardinalities, access-path choice) lives on `Database` in `lib.rs`.

use sql::ast::{AggFunc, BinOp, Expr};

use crate::plan::{resolve_col, Col};
use crate::EngineError;

/// Collect the distinct aggregate expressions appearing anywhere in `e`.
pub fn collect_aggs_in(e: &Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Aggregate { .. } => {
            if !out.contains(e) {
                out.push(e.clone());
            }
        }
        Expr::Binary { left, right, .. } => {
            collect_aggs_in(left, out);
            collect_aggs_in(right, out);
        }
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => collect_aggs_in(expr, out),
        _ => {}
    }
}

/// Replace any subexpression equal to a `subs` key with its replacement. Whole
/// aggregate/group expressions are matched as units (no recursion into them).
pub fn substitute(e: &Expr, subs: &[(Expr, Expr)]) -> Expr {
    for (from, to) in subs {
        if e == from {
            return to.clone();
        }
    }
    match e {
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(substitute(left, subs)),
            right: Box::new(substitute(right, subs)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(substitute(expr, subs)),
        },
        Expr::IsNull { expr, negated } => Expr::IsNull {
            expr: Box::new(substitute(expr, subs)),
            negated: *negated,
        },
        other => other.clone(),
    }
}

/// The default output column name for an (unaliased) projection expression.
pub fn default_name(e: &Expr) -> String {
    match e {
        Expr::Column { name, .. } => name.clone(),
        Expr::Aggregate { func, .. } => match func {
            AggFunc::Count => "count",
            AggFunc::Sum => "sum",
            AggFunc::Avg => "avg",
            AggFunc::Min => "min",
            AggFunc::Max => "max",
        }
        .to_string(),
        _ => "expr".to_string(),
    }
}

/// Check that every column reference in `e` resolves within `scope`, at plan time.
pub fn validate_cols(e: &Expr, scope: &[Col]) -> Result<(), EngineError> {
    match e {
        Expr::Column { table, name } => {
            resolve_col(scope, table.as_deref(), name)?;
            Ok(())
        }
        Expr::Binary { left, right, .. } => {
            validate_cols(left, scope)?;
            validate_cols(right, scope)
        }
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => validate_cols(expr, scope),
        Expr::Aggregate { arg, .. } => match arg {
            Some(a) => validate_cols(a, scope),
            None => Ok(()),
        },
        Expr::Literal(_) => Ok(()),
    }
}

/// True if every column reference in `e` resolves within `cols`.
pub fn expr_resolves(e: &Expr, cols: &[Col]) -> bool {
    match e {
        Expr::Column { table, name } => resolve_col(cols, table.as_deref(), name).is_ok(),
        Expr::Literal(_) => true,
        Expr::Binary { left, right, .. } => expr_resolves(left, cols) && expr_resolves(right, cols),
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => expr_resolves(expr, cols),
        Expr::Aggregate { .. } => false,
    }
}

/// If `on` is a top-level equijoin connecting the two sides, return the
/// `(left_key, right_key)` pair oriented to the left/right schemas (for hash join).
pub fn detect_hash_keys(on: &Expr, left: &[Col], right: &[Col]) -> Option<(Expr, Expr)> {
    if let Expr::Binary {
        op: BinOp::Eq,
        left: l,
        right: r,
    } = on
    {
        if expr_resolves(l, left) && expr_resolves(r, right) {
            return Some(((**l).clone(), (**r).clone()));
        }
        if expr_resolves(l, right) && expr_resolves(r, left) {
            return Some(((**r).clone(), (**l).clone()));
        }
    }
    None
}
