//! End-to-end vector search (M9): `VECTOR(dim)` columns, `CREATE INDEX ...
//! USING HNSW`, `ORDER BY distance(col, '[...]') LIMIT k` through the real
//! SQL path, filtered search, MVCC ghosts, and reopen behavior.

use engine::{Database, Output, SqlValue as Value};

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn ids(db: &mut Database, sql: &str) -> Vec<i64> {
    rows(db, sql)
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(x) => x,
            ref v => panic!("expected integer id, got {v:?}"),
        })
        .collect()
}

/// items: id i, category alternating 'a'/'b', embedding [i, 0, 0].
/// Distances to the origin are exactly i² — ordering is unambiguous.
fn setup(db: &mut Database, n: i64) {
    db.execute("CREATE TABLE items (id INTEGER PRIMARY KEY, category TEXT, embedding VECTOR(3))")
        .unwrap();
    for i in 0..n {
        let cat = if i % 2 == 0 { "a" } else { "b" };
        db.execute(&format!(
            "INSERT INTO items VALUES ({i}, '{cat}', '[{i}, 0, 0]')"
        ))
        .unwrap();
    }
}

#[test]
fn knn_without_index_is_exact() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 50);
    let got = ids(
        &mut db,
        "SELECT id FROM items ORDER BY distance(embedding, '[19.6, 0, 0]') LIMIT 3",
    );
    assert_eq!(got, vec![20, 19, 21]);
}

#[test]
fn knn_with_index_matches_exact_and_explain_shows_it() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 200);
    let q = "SELECT id FROM items ORDER BY distance(embedding, '[123.4, 0, 0]') LIMIT 5";
    let exact = ids(&mut db, q);

    db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
        .unwrap();

    // EXPLAIN must show the index access path.
    let plan = rows(&mut db, &format!("EXPLAIN {q}"));
    let text: String = plan
        .iter()
        .map(|r| match &r[0] {
            Value::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("HnswTopK"), "plan was:\n{text}");

    // And the results must agree with the exact scan (this dataset is easy
    // enough that recall is 1.0; the Sort above the scan guarantees order).
    assert_eq!(ids(&mut db, q), exact);
}

#[test]
fn insert_validates_dimension() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 1);
    let err = db
        .execute("INSERT INTO items VALUES (99, 'a', '[1, 2]')")
        .unwrap_err();
    assert!(err.to_string().contains("VECTOR(3)"), "got: {err}");
}

#[test]
fn create_index_rejects_non_vector_columns() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 1);
    let err = db
        .execute("CREATE INDEX bad ON items USING HNSW (category)")
        .unwrap_err();
    assert!(err.to_string().contains("VECTOR"), "got: {err}");
}

#[test]
fn filtered_knn_respects_predicate_and_matches_exact() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 200);
    let q = "SELECT id FROM items WHERE category = 'b' \
             ORDER BY distance(embedding, '[100.2, 0, 0]') LIMIT 4";
    // Exact answer first (no index): nearest odd ids to 100.2.
    let exact = ids(&mut db, q);
    assert_eq!(exact, vec![101, 99, 103, 97]);

    db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
        .unwrap();
    // Predicate-aware traversal must reproduce it.
    assert_eq!(ids(&mut db, q), exact);
}

#[test]
fn ultra_selective_filter_still_finds_the_needle() {
    // The recall-cliff scenario: only ONE row matches the predicate. A naive
    // post-filter over top-k would almost surely return nothing; the engine's
    // ef-escalation must degenerate into an exhaustive filtered search and
    // find the needle.
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 300);
    db.execute("INSERT INTO items VALUES (1000, 'rare', '[0.5, 0, 0]')")
        .unwrap();
    db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
        .unwrap();
    let got = ids(
        &mut db,
        "SELECT id FROM items WHERE category = 'rare' \
         ORDER BY distance(embedding, '[250, 0, 0]') LIMIT 3",
    );
    assert_eq!(got, vec![1000]);
}

#[test]
fn aborted_insert_is_a_ghost_the_search_never_returns() {
    // The index is not MVCC-aware: the aborted row's vector stays in the
    // graph as a ghost. Visibility filtering at the B+-tree fetch must drop
    // it — this is the Postgres-parallel behavior, verified end to end.
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 20);
    db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
        .unwrap();

    let txn = db.begin();
    db.execute_in(txn, "INSERT INTO items VALUES (777, 'a', '[5.1, 0, 0]')")
        .unwrap();
    db.rollback_txn(txn).unwrap();

    let got = ids(
        &mut db,
        "SELECT id FROM items ORDER BY distance(embedding, '[5.1, 0, 0]') LIMIT 3",
    );
    assert!(!got.contains(&777), "ghost row returned: {got:?}");
    assert_eq!(got[0], 5); // the true nearest visible row
}

#[test]
fn deleted_rows_disappear_from_search() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 30);
    db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
        .unwrap();
    db.execute("DELETE FROM items WHERE id = 10").unwrap();
    let got = ids(
        &mut db,
        "SELECT id FROM items ORDER BY distance(embedding, '[10, 0, 0]') LIMIT 3",
    );
    assert!(!got.contains(&10), "deleted row returned: {got:?}");
    assert_eq!(got[0], 9); // 9 and 11 tie at distance 1; either order ok
}

#[test]
fn updated_vector_is_reindexed() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 30);
    db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
        .unwrap();
    // Move row 5 far away; it must stop matching near 5 and start near 500.
    db.execute("UPDATE items SET embedding = '[500, 0, 0]' WHERE id = 5")
        .unwrap();
    let near_5 = ids(
        &mut db,
        "SELECT id FROM items ORDER BY distance(embedding, '[5, 0, 0]') LIMIT 2",
    );
    assert!(!near_5.contains(&5), "stale index entry: {near_5:?}");
    let near_500 = ids(
        &mut db,
        "SELECT id FROM items ORDER BY distance(embedding, '[500, 0, 0]') LIMIT 1",
    );
    assert_eq!(near_500, vec![5]);
}

#[test]
fn index_survives_reopen_via_sidecar_or_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vec.db");
    {
        let mut db = Database::open(&path).unwrap();
        setup(&mut db, 60);
        db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
            .unwrap();
        db.checkpoint().unwrap(); // persists sidecar
    }
    {
        // Warm start: sidecar is fresh, loads directly (mmap on unix).
        let mut db = Database::open(&path).unwrap();
        let got = ids(
            &mut db,
            "SELECT id FROM items ORDER BY distance(embedding, '[30.3, 0, 0]') LIMIT 2",
        );
        assert_eq!(got, vec![30, 31]);
    }
    {
        // Cold start: delete the sidecar; the index must rebuild from the
        // table (the "index is derived data" guarantee).
        let sidecar = dir.path().join("vec.db.hnsw-items-embedding");
        std::fs::remove_file(&sidecar).unwrap();
        let mut db = Database::open(&path).unwrap();
        let got = ids(
            &mut db,
            "SELECT id FROM items ORDER BY distance(embedding, '[30.3, 0, 0]') LIMIT 2",
        );
        assert_eq!(got, vec![30, 31]);
    }
}

#[test]
fn stale_sidecar_is_rebuilt_not_trusted() {
    // Insert after the last checkpoint, then reopen: the sidecar undercounts
    // and must be discarded in favor of a rebuild — otherwise the newest row
    // would be invisible to vector search.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.db");
    {
        let mut db = Database::open(&path).unwrap();
        setup(&mut db, 20);
        db.execute("CREATE INDEX items_emb ON items USING HNSW (embedding)")
            .unwrap();
        db.checkpoint().unwrap();
        // One more insert, NO checkpoint: sidecar is now stale.
        db.execute("INSERT INTO items VALUES (99, 'a', '[99, 0, 0]')")
            .unwrap();
    }
    {
        let mut db = Database::open(&path).unwrap();
        let got = ids(
            &mut db,
            "SELECT id FROM items ORDER BY distance(embedding, '[99, 0, 0]') LIMIT 1",
        );
        assert_eq!(got, vec![99], "stale sidecar served; rebuild did not fire");
    }
}

#[test]
fn distance_in_projection_works() {
    let mut db = Database::open_in_memory().unwrap();
    setup(&mut db, 10);
    let r = rows(
        &mut db,
        "SELECT id, distance(embedding, '[0, 0, 0]') AS d FROM items \
         ORDER BY distance(embedding, '[0, 0, 0]') LIMIT 3",
    );
    // Squared L2: 0, 1, 4.
    assert_eq!(r[0][1], Value::Real(0.0));
    assert_eq!(r[1][1], Value::Real(1.0));
    assert_eq!(r[2][1], Value::Real(4.0));
}
