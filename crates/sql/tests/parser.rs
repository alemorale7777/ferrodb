use sql::ast::*;
use sql::{parse, SqlError};

fn col(name: &str) -> Expr {
    Expr::col(name)
}
fn qcol(table: &str, name: &str) -> Expr {
    Expr::Column {
        table: Some(table.into()),
        name: name.into(),
    }
}
fn int(n: i64) -> Expr {
    Expr::Literal(Value::Integer(n))
}
fn bin(op: BinOp, l: Expr, r: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}
fn item(e: Expr) -> SelectItem {
    SelectItem::Expr {
        expr: e,
        alias: None,
    }
}
fn tref(name: &str) -> TableRef {
    TableRef {
        name: name.into(),
        alias: None,
    }
}
fn as_select(s: Statement) -> Select {
    match s {
        Statement::Select(sel) => sel,
        other => panic!("not a select: {other:?}"),
    }
}

#[test]
fn parses_create_table() {
    let s = parse("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER)")
        .unwrap();
    assert_eq!(
        s,
        Statement::CreateTable {
            name: "users".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::Integer,
                    not_null: true,
                    primary_key: true
                },
                ColumnDef {
                    name: "name".into(),
                    data_type: DataType::Text,
                    not_null: true,
                    primary_key: false
                },
                ColumnDef {
                    name: "age".into(),
                    data_type: DataType::Integer,
                    not_null: false,
                    primary_key: false
                },
            ],
        }
    );
}

#[test]
fn parses_multi_row_insert() {
    let s = parse("INSERT INTO users (id, name) VALUES (1, 'al'), (2, 'sam')").unwrap();
    assert_eq!(
        s,
        Statement::Insert {
            table: "users".into(),
            columns: Some(vec!["id".into(), "name".into()]),
            rows: vec![
                vec![int(1), Expr::Literal(Value::Text("al".into()))],
                vec![int(2), Expr::Literal(Value::Text("sam".into()))],
            ],
        }
    );
}

#[test]
fn parses_select_with_where_order_limit() {
    let sel = as_select(
        parse("SELECT name, age FROM users WHERE age > 26 ORDER BY name DESC LIMIT 10 OFFSET 5")
            .unwrap(),
    );
    assert_eq!(sel.items, vec![item(col("name")), item(col("age"))]);
    assert_eq!(sel.from, tref("users"));
    assert!(sel.joins.is_empty());
    assert_eq!(sel.filter, Some(bin(BinOp::Gt, col("age"), int(26))));
    assert_eq!(
        sel.order_by,
        vec![OrderBy {
            expr: col("name"),
            descending: true
        }]
    );
    assert_eq!(sel.limit, Some(10));
    assert_eq!(sel.offset, Some(5));
}

#[test]
fn select_star() {
    let sel = as_select(parse("SELECT * FROM t").unwrap());
    assert_eq!(sel.items, vec![SelectItem::Wildcard]);
}

#[test]
fn qualified_columns_and_aliases_and_inner_join() {
    let sel = as_select(
        parse("SELECT u.name, o.total FROM users u JOIN orders AS o ON u.id = o.user_id").unwrap(),
    );
    assert_eq!(
        sel.items,
        vec![item(qcol("u", "name")), item(qcol("o", "total"))]
    );
    assert_eq!(
        sel.from,
        TableRef {
            name: "users".into(),
            alias: Some("u".into())
        }
    );
    assert_eq!(sel.joins.len(), 1);
    let j = &sel.joins[0];
    assert_eq!(j.join_type, JoinType::Inner);
    assert_eq!(
        j.right,
        TableRef {
            name: "orders".into(),
            alias: Some("o".into())
        }
    );
    assert_eq!(j.on, bin(BinOp::Eq, qcol("u", "id"), qcol("o", "user_id")));
}

#[test]
fn left_outer_join_is_left() {
    let sel = as_select(parse("SELECT * FROM a LEFT OUTER JOIN b ON a.k = b.k").unwrap());
    assert_eq!(sel.joins[0].join_type, JoinType::Left);
}

#[test]
fn projection_alias_with_as_and_bare() {
    let sel = as_select(parse("SELECT age AS years, name handle FROM t").unwrap());
    assert_eq!(
        sel.items,
        vec![
            SelectItem::Expr {
                expr: col("age"),
                alias: Some("years".into())
            },
            SelectItem::Expr {
                expr: col("name"),
                alias: Some("handle".into())
            },
        ]
    );
}

#[test]
fn aggregates_group_by_having() {
    let sel = as_select(
        parse("SELECT dept, COUNT(*), SUM(salary) FROM emp GROUP BY dept HAVING COUNT(*) > 2")
            .unwrap(),
    );
    assert_eq!(
        sel.items,
        vec![
            item(col("dept")),
            item(Expr::Aggregate {
                func: AggFunc::Count,
                arg: None
            }),
            item(Expr::Aggregate {
                func: AggFunc::Sum,
                arg: Some(Box::new(col("salary")))
            }),
        ]
    );
    assert_eq!(sel.group_by, vec![col("dept")]);
    assert_eq!(
        sel.having,
        Some(bin(
            BinOp::Gt,
            Expr::Aggregate {
                func: AggFunc::Count,
                arg: None
            },
            int(2)
        ))
    );
}

#[test]
fn qualified_wildcard_and_multi_key_order() {
    let sel = as_select(parse("SELECT t.* FROM t ORDER BY a ASC, b DESC").unwrap());
    assert_eq!(sel.items, vec![SelectItem::QualifiedWildcard("t".into())]);
    assert_eq!(
        sel.order_by,
        vec![
            OrderBy {
                expr: col("a"),
                descending: false
            },
            OrderBy {
                expr: col("b"),
                descending: true
            },
        ]
    );
}

#[test]
fn explain_wraps_a_select() {
    let s = parse("EXPLAIN SELECT * FROM t WHERE id = 1").unwrap();
    let Statement::Explain(inner) = s else {
        panic!("not explain");
    };
    let sel = as_select(*inner);
    assert_eq!(sel.filter, Some(bin(BinOp::Eq, col("id"), int(1))));
}

#[test]
fn and_binds_tighter_than_or() {
    let sel = as_select(parse("SELECT * FROM t WHERE a OR b AND c").unwrap());
    assert_eq!(
        sel.filter,
        Some(bin(
            BinOp::Or,
            col("a"),
            bin(BinOp::And, col("b"), col("c"))
        ))
    );
}

#[test]
fn comparison_binds_tighter_than_not() {
    let sel = as_select(parse("SELECT * FROM t WHERE NOT a = b").unwrap());
    assert_eq!(
        sel.filter,
        Some(Expr::Unary {
            op: UnOp::Not,
            expr: Box::new(bin(BinOp::Eq, col("a"), col("b"))),
        })
    );
}

#[test]
fn arithmetic_precedence() {
    let sel = as_select(parse("SELECT * FROM t WHERE x = 1 + 2 * 3").unwrap());
    assert_eq!(
        sel.filter,
        Some(bin(
            BinOp::Eq,
            col("x"),
            bin(BinOp::Add, int(1), bin(BinOp::Mul, int(2), int(3)))
        ))
    );
}

#[test]
fn is_null_and_is_not_null() {
    let sel = as_select(parse("SELECT * FROM t WHERE a IS NULL").unwrap());
    assert_eq!(
        sel.filter,
        Some(Expr::IsNull {
            expr: Box::new(col("a")),
            negated: false
        })
    );

    let sel = as_select(parse("SELECT * FROM t WHERE a IS NOT NULL").unwrap());
    assert_eq!(
        sel.filter,
        Some(Expr::IsNull {
            expr: Box::new(col("a")),
            negated: true
        })
    );
}

#[test]
fn parses_update_and_delete_and_drop() {
    assert_eq!(
        parse("UPDATE users SET age = 31 WHERE id = 1").unwrap(),
        Statement::Update {
            table: "users".into(),
            assignments: vec![("age".into(), int(31))],
            filter: Some(bin(BinOp::Eq, col("id"), int(1))),
        }
    );
    assert_eq!(
        parse("DELETE FROM users WHERE id = 2").unwrap(),
        Statement::Delete {
            table: "users".into(),
            filter: Some(bin(BinOp::Eq, col("id"), int(2))),
        }
    );
    assert_eq!(
        parse("DROP TABLE users").unwrap(),
        Statement::DropTable {
            name: "users".into()
        }
    );
}

#[test]
fn trailing_semicolon_ok_and_garbage_rejected() {
    assert!(parse("SELECT * FROM t;").is_ok());
    // a number after the table ref is neither an alias nor a clause keyword
    assert!(matches!(
        parse("SELECT * FROM t 123"),
        Err(SqlError::Parse(_))
    ));
    assert!(matches!(parse("SELECT FROM"), Err(SqlError::Parse(_))));
}
