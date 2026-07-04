//! M5 query-engine tests: joins, aggregation, grouping, and EXPLAIN.

use engine::{Database, Output};
use sql::ast::Value;

fn fresh_db() -> Database {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    std::mem::forget(dir);
    Database::open(path).unwrap()
}

fn rows(out: Output) -> Vec<Vec<Value>> {
    match out {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}
fn cols(out: &Output) -> Vec<String> {
    match out {
        Output::Rows { columns, .. } => columns.clone(),
        other => panic!("expected rows, got {other:?}"),
    }
}
fn int(n: i64) -> Value {
    Value::Integer(n)
}
fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn users_and_orders() -> Database {
    let mut db = fresh_db();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, total INTEGER)")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'al'), (2, 'sam'), (3, 'kai')")
        .unwrap();
    db.execute("INSERT INTO orders VALUES (10, 1, 100), (11, 1, 50), (12, 2, 70)")
        .unwrap();
    db
}

#[test]
fn in_memory_database_runs_the_full_stack() {
    // No filesystem — the same path the WASM build uses.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    db.execute("UPDATE t SET v = v + 1 WHERE id = 2").unwrap();
    let out = db
        .execute("SELECT SUM(v) AS s FROM t WHERE id >= 2")
        .unwrap();
    assert_eq!(rows(out), vec![vec![int(51)]]); // 21 + 30
}

#[test]
fn inner_join_matches_rows() {
    let mut db = users_and_orders();
    let out = db
        .execute("SELECT u.name, o.total FROM users u JOIN orders o ON u.id = o.user_id ORDER BY o.total")
        .unwrap();
    assert_eq!(cols(&out), vec!["name", "total"]);
    assert_eq!(
        rows(out),
        vec![
            vec![text("al"), int(50)],
            vec![text("sam"), int(70)],
            vec![text("al"), int(100)],
        ]
    );
}

#[test]
fn left_join_null_pads_unmatched() {
    let mut db = users_and_orders();
    // kai (id=3) has no orders -> total is NULL
    let out = db
        .execute("SELECT u.name, o.total FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.id = 3")
        .unwrap();
    assert_eq!(rows(out), vec![vec![text("kai"), Value::Null]]);
}

#[test]
fn three_table_join() {
    let mut db = users_and_orders();
    db.execute("CREATE TABLE items (order_id INTEGER, sku TEXT)")
        .unwrap();
    db.execute("INSERT INTO items VALUES (10, 'A'), (10, 'B'), (11, 'C')")
        .unwrap();
    let out = db
        .execute(
            "SELECT u.name, i.sku FROM users u \
             JOIN orders o ON u.id = o.user_id \
             JOIN items i ON o.id = i.order_id \
             ORDER BY i.sku",
        )
        .unwrap();
    assert_eq!(
        rows(out),
        vec![
            vec![text("al"), text("A")],
            vec![text("al"), text("B")],
            vec![text("al"), text("C")],
        ]
    );
}

#[test]
fn global_aggregates() {
    let mut db = users_and_orders();
    let out = db
        .execute("SELECT COUNT(*), SUM(total), MIN(total), MAX(total), AVG(total) FROM orders")
        .unwrap();
    assert_eq!(
        rows(out),
        vec![vec![
            int(3),
            int(220),
            int(50),
            int(100),
            Value::Real(220.0 / 3.0)
        ]]
    );
}

#[test]
fn group_by_with_having() {
    let mut db = users_and_orders();
    // orders per user, keeping only users with more than one order
    let out = db
        .execute(
            "SELECT user_id, COUNT(*) AS n, SUM(total) AS spent FROM orders \
             GROUP BY user_id HAVING COUNT(*) > 1 ORDER BY user_id",
        )
        .unwrap();
    assert_eq!(cols(&out), vec!["user_id", "n", "spent"]);
    assert_eq!(rows(out), vec![vec![int(1), int(2), int(150)]]);
}

#[test]
fn order_by_aggregate_alias() {
    let mut db = users_and_orders();
    // al spent 150, sam spent 70; order by the SUM alias descending
    let out = db
        .execute(
            "SELECT u.name, SUM(o.total) AS spent FROM users u \
             JOIN orders o ON u.id = o.user_id GROUP BY u.name ORDER BY spent DESC",
        )
        .unwrap();
    assert_eq!(
        rows(out),
        vec![vec![text("al"), int(150)], vec![text("sam"), int(70)]]
    );
}

#[test]
fn count_skips_nulls_but_count_star_does_not() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t (id) VALUES (1)").unwrap(); // v NULL
    db.execute("INSERT INTO t VALUES (2, 5), (3, 7)").unwrap();
    let out = db
        .execute("SELECT COUNT(*), COUNT(v), SUM(v) FROM t")
        .unwrap();
    assert_eq!(rows(out), vec![vec![int(3), int(2), int(12)]]);
}

#[test]
fn global_aggregate_over_empty_table() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    let out = db.execute("SELECT COUNT(*), SUM(v) FROM t").unwrap();
    assert_eq!(rows(out), vec![vec![int(0), Value::Null]]);
}

fn explain_text(db: &mut Database, sql: &str) -> String {
    rows(db.execute(sql).unwrap())
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            _ => panic!(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn index_seek_for_primary_key_equality() {
    let mut db = users_and_orders();
    let seek = explain_text(&mut db, "EXPLAIN SELECT * FROM users WHERE id = 2");
    assert!(
        seek.contains("IndexSeek users (pk = 2)"),
        "plan was:\n{seek}"
    );
    // a non-PK predicate cannot use the index
    let scan = explain_text(&mut db, "EXPLAIN SELECT * FROM users WHERE name = 'al'");
    assert!(scan.contains("SeqScan users"), "plan was:\n{scan}");
    assert!(scan.contains("filter: name = 'al'"), "plan was:\n{scan}");
    // and it still returns the right row
    assert_eq!(
        rows(db.execute("SELECT name FROM users WHERE id = 2").unwrap()),
        vec![vec![text("sam")]]
    );
}

#[test]
fn single_table_predicate_is_pushed_to_the_scan() {
    let mut db = users_and_orders();
    let plan = explain_text(
        &mut db,
        "EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id WHERE u.name = 'al'",
    );
    // the u-only predicate rides on the users scan, not a separate Filter node
    assert!(plan.contains("filter: u.name = 'al'"), "plan was:\n{plan}");
}

#[test]
fn join_order_avoids_leading_with_the_big_table() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE big (id INTEGER PRIMARY KEY, k INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE mid (id INTEGER PRIMARY KEY, k INTEGER, j INTEGER)")
        .unwrap();
    db.execute("CREATE TABLE small (id INTEGER PRIMARY KEY, j INTEGER)")
        .unwrap();
    for i in 1..=100 {
        db.execute(&format!("INSERT INTO big VALUES ({i}, {})", i % 10))
            .unwrap();
    }
    for i in 1..=10 {
        db.execute(&format!("INSERT INTO mid VALUES ({i}, {}, {i})", i % 10))
            .unwrap();
    }
    db.execute("INSERT INTO small VALUES (1, 5)").unwrap();

    // chain: big.k = mid.k, mid.j = small.j
    let plan = explain_text(
        &mut db,
        "EXPLAIN SELECT big.id FROM big \
         JOIN mid ON big.k = mid.k JOIN small ON mid.j = small.j",
    );
    let big_at = plan.find("SeqScan big").expect("big scanned");
    let small_at = plan.find("SeqScan small").expect("small scanned");
    // leading with the 100-row table is the expensive plan; the optimizer avoids it
    assert!(small_at < big_at, "optimizer led with big:\n{plan}");
}

#[test]
fn explain_shows_join_and_scans() {
    let mut db = users_and_orders();
    let out = db
        .execute("EXPLAIN SELECT u.name FROM users u JOIN orders o ON u.id = o.user_id")
        .unwrap();
    let text: String = rows(out)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            _ => panic!(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("HashJoin"), "plan was:\n{text}");
    assert!(text.contains("SeqScan users"), "plan was:\n{text}");
    assert!(text.contains("SeqScan orders"), "plan was:\n{text}");
}
