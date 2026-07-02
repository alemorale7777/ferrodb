use sql::ast::*;
use sql::{parse, SqlError};

fn col(name: &str) -> Expr {
    Expr::Column(name.into())
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
    let s =
        parse("SELECT name, age FROM users WHERE age > 26 ORDER BY name DESC LIMIT 10 OFFSET 5")
            .unwrap();
    let Statement::Select {
        items,
        from,
        filter,
        order_by,
        limit,
        offset,
    } = s
    else {
        panic!("not a select");
    };
    assert_eq!(
        items,
        vec![
            SelectItem::Column("name".into()),
            SelectItem::Column("age".into())
        ]
    );
    assert_eq!(from, "users");
    assert_eq!(filter, Some(bin(BinOp::Gt, col("age"), int(26))));
    assert_eq!(
        order_by,
        Some(OrderBy {
            column: "name".into(),
            descending: true
        })
    );
    assert_eq!(limit, Some(10));
    assert_eq!(offset, Some(5));
}

#[test]
fn select_star() {
    let s = parse("SELECT * FROM t").unwrap();
    let Statement::Select { items, .. } = s else {
        panic!()
    };
    assert_eq!(items, vec![SelectItem::Wildcard]);
}

#[test]
fn and_binds_tighter_than_or() {
    // a OR b AND c  ==  a OR (b AND c)
    let s = parse("SELECT * FROM t WHERE a OR b AND c").unwrap();
    let Statement::Select { filter, .. } = s else {
        panic!()
    };
    assert_eq!(
        filter,
        Some(bin(
            BinOp::Or,
            col("a"),
            bin(BinOp::And, col("b"), col("c"))
        ))
    );
}

#[test]
fn comparison_binds_tighter_than_not() {
    // NOT a = b  ==  NOT (a = b)
    let s = parse("SELECT * FROM t WHERE NOT a = b").unwrap();
    let Statement::Select { filter, .. } = s else {
        panic!()
    };
    assert_eq!(
        filter,
        Some(Expr::Unary {
            op: UnOp::Not,
            expr: Box::new(bin(BinOp::Eq, col("a"), col("b"))),
        })
    );
}

#[test]
fn arithmetic_precedence() {
    // 1 + 2 * 3  ==  1 + (2 * 3)
    let s = parse("SELECT * FROM t WHERE x = 1 + 2 * 3").unwrap();
    let Statement::Select { filter, .. } = s else {
        panic!()
    };
    assert_eq!(
        filter,
        Some(bin(
            BinOp::Eq,
            col("x"),
            bin(BinOp::Add, int(1), bin(BinOp::Mul, int(2), int(3)))
        ))
    );
}

#[test]
fn is_null_and_is_not_null() {
    let s = parse("SELECT * FROM t WHERE a IS NULL").unwrap();
    let Statement::Select { filter, .. } = s else {
        panic!()
    };
    assert_eq!(
        filter,
        Some(Expr::IsNull {
            expr: Box::new(col("a")),
            negated: false
        })
    );

    let s = parse("SELECT * FROM t WHERE a IS NOT NULL").unwrap();
    let Statement::Select { filter, .. } = s else {
        panic!()
    };
    assert_eq!(
        filter,
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
    assert!(matches!(
        parse("SELECT * FROM t GARBAGE"),
        Err(SqlError::Parse(_))
    ));
    assert!(matches!(parse("SELECT FROM"), Err(SqlError::Parse(_))));
}
