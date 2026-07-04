//! MVCC transaction behaviour: snapshot isolation, rollback, write conflicts, vacuum.

use engine::{Database, EngineError, Output};
use sql::ast::Value;

fn fresh_db() -> Database {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    std::mem::forget(dir);
    Database::open(path).unwrap()
}

fn vs(out: Output) -> Vec<i64> {
    match out {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Integer(x) => x,
                ref other => panic!("expected int, got {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn snapshot_isolation_hides_uncommitted_and_freezes_the_view() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();

    let t1 = db.begin();
    db.execute_in(t1, "INSERT INTO t VALUES (1, 10)").unwrap();

    // t2's snapshot is taken while t1 is still open.
    let t2 = db.begin();
    assert_eq!(
        vs(db.execute_in(t2, "SELECT v FROM t").unwrap()),
        Vec::<i64>::new()
    );

    // Even after t1 commits, t2's frozen snapshot still doesn't see the row.
    db.commit_txn(t1).unwrap();
    assert_eq!(
        vs(db.execute_in(t2, "SELECT v FROM t").unwrap()),
        Vec::<i64>::new()
    );
    db.commit_txn(t2).unwrap();

    // A transaction that begins after the commit does see it.
    let t3 = db.begin();
    assert_eq!(vs(db.execute_in(t3, "SELECT v FROM t").unwrap()), vec![10]);
    db.commit_txn(t3).unwrap();
}

#[test]
fn rollback_undoes_inserts_updates_and_deletes() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    let t1 = db.begin();
    db.execute_in(t1, "UPDATE t SET v = 99 WHERE id = 1")
        .unwrap();
    db.execute_in(t1, "INSERT INTO t VALUES (2, 20)").unwrap();
    db.execute_in(t1, "SELECT v FROM t ORDER BY id").unwrap();
    db.rollback_txn(t1).unwrap();

    // Nothing t1 did survives: the original row is intact, the insert is gone.
    assert_eq!(
        vs(db.execute("SELECT v FROM t ORDER BY id").unwrap()),
        vec![10]
    );
}

#[test]
fn rolled_back_delete_leaves_the_row_live() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    let t1 = db.begin();
    db.execute_in(t1, "DELETE FROM t WHERE id = 1").unwrap();
    db.rollback_txn(t1).unwrap();

    assert_eq!(vs(db.execute("SELECT v FROM t").unwrap()), vec![10]);
}

#[test]
fn concurrent_update_of_the_same_row_is_a_write_conflict() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)").unwrap();

    let t1 = db.begin();
    let t2 = db.begin();
    db.execute_in(t1, "UPDATE t SET v = 20 WHERE id = 1")
        .unwrap();
    db.commit_txn(t1).unwrap();

    // t2 saw the original row; its write now collides with t1's committed update.
    assert!(matches!(
        db.execute_in(t2, "UPDATE t SET v = 30 WHERE id = 1"),
        Err(EngineError::Conflict(_))
    ));
    db.rollback_txn(t2).unwrap();

    // First-updater-wins: t1's value stands.
    assert_eq!(vs(db.execute("SELECT v FROM t").unwrap()), vec![20]);
}

#[test]
fn vacuum_reclaims_dead_versions_but_keeps_live_data() {
    let mut db = fresh_db();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    // id=1 gets a superseded version; id=2 gets fully deleted.
    db.execute("UPDATE t SET v = 11 WHERE id = 1").unwrap();
    db.execute("DELETE FROM t WHERE id = 2").unwrap();

    let removed = match db.vacuum().unwrap() {
        Output::Affected(n) => n,
        other => panic!("expected affected, got {other:?}"),
    };
    // The old id=1 version and both id=2 versions are dead.
    assert!(
        removed >= 2,
        "expected to reclaim dead versions, got {removed}"
    );

    // Live data is untouched.
    assert_eq!(
        vs(db.execute("SELECT v FROM t ORDER BY id").unwrap()),
        vec![11]
    );
}
