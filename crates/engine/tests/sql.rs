use engine::{Database, EngineError, Output};
use sql::ast::Value;

fn text(s: &str) -> Value {
    Value::Text(s.into())
}
fn int(n: i64) -> Value {
    Value::Integer(n)
}

fn fresh_db() -> Database {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    std::mem::forget(dir); // keep the file alive for the test
    Database::open(path).unwrap()
}

fn rows(out: Output) -> (Vec<String>, Vec<Vec<Value>>) {
    match out {
        Output::Rows { columns, rows } => (columns, rows),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn create_insert_select_where_order() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();
    assert_eq!(
        db.execute("INSERT INTO users VALUES (1, 'alejandro', 30), (2, 'sam', 25), (3, 'kai', 40)")
            .unwrap(),
        Output::Affected(3)
    );
    let (cols, r) = rows(
        db.execute("SELECT name, age FROM users WHERE age > 26 ORDER BY name")
            .unwrap(),
    );
    assert_eq!(cols, vec!["name", "age"]);
    assert_eq!(
        r,
        vec![vec![text("alejandro"), int(30)], vec![text("kai"), int(40)],]
    );
}

#[test]
fn select_star_and_limit_offset() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    for i in 1..=5 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 10))
            .unwrap();
    }
    // primary-key order is the physical scan order
    let (cols, r) = rows(
        db.execute("SELECT * FROM t ORDER BY id LIMIT 2 OFFSET 1")
            .unwrap(),
    );
    assert_eq!(cols, vec!["id", "v"]);
    assert_eq!(r, vec![vec![int(2), int(20)], vec![int(3), int(30)]]);
}

#[test]
fn update_and_delete() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
        .unwrap();
    assert_eq!(
        db.execute("UPDATE t SET v = v + 100 WHERE id >= 2")
            .unwrap(),
        Output::Affected(2)
    );
    assert_eq!(
        db.execute("DELETE FROM t WHERE id = 1").unwrap(),
        Output::Affected(1)
    );
    let (_c, r) = rows(db.execute("SELECT id, v FROM t ORDER BY id").unwrap());
    assert_eq!(r, vec![vec![int(2), int(120)], vec![int(3), int(130)]]);
}

#[test]
fn null_and_three_valued_logic() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT)")
        .unwrap();
    db.execute("INSERT INTO t (id) VALUES (1)").unwrap(); // note is NULL
    db.execute("INSERT INTO t VALUES (2, 'hi')").unwrap();
    let (_c, r) = rows(db.execute("SELECT id FROM t WHERE note IS NULL").unwrap());
    assert_eq!(r, vec![vec![int(1)]]);
    let (_c, r) = rows(
        db.execute("SELECT id FROM t WHERE note IS NOT NULL")
            .unwrap(),
    );
    assert_eq!(r, vec![vec![int(2)]]);
    // a comparison against NULL is NULL (not true) → row excluded
    let (_c, r) = rows(db.execute("SELECT id FROM t WHERE note = 'hi'").unwrap());
    assert_eq!(r, vec![vec![int(2)]]);
}

#[test]
fn pkless_table_uses_rowid() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE log (msg TEXT)").unwrap();
    db.execute("INSERT INTO log VALUES ('a'), ('b'), ('c')")
        .unwrap();
    let (_c, r) = rows(db.execute("SELECT msg FROM log").unwrap());
    assert_eq!(r, vec![vec![text("a")], vec![text("b")], vec![text("c")]]);
}

#[test]
fn errors_are_reported_not_panicked() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();
    assert!(matches!(
        db.execute("SELECT * FROM nope"),
        Err(EngineError::UnknownTable(_))
    ));
    assert!(matches!(
        db.execute("SELECT bogus FROM t"),
        Err(EngineError::UnknownColumn(_))
    ));
    assert!(matches!(
        db.execute("INSERT INTO t VALUES (1, 42)"),
        Err(EngineError::Type(_))
    ));
    assert!(matches!(
        db.execute("INSERT INTO t (id) VALUES (1)"),
        Err(EngineError::Constraint(_))
    ));
}

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.db");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
            .unwrap();
        db.checkpoint().unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let (_c, r) = rows(db.execute("SELECT name FROM t ORDER BY id").unwrap());
        assert_eq!(r, vec![vec![text("one")], vec![text("two")]]);
    }
}
