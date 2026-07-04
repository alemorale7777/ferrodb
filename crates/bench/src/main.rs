//! Micro-benchmarks for ferrodb, optionally compared against SQLite.
//!
//! Both engines run fully in memory, driven by SQL strings (no prepared-statement
//! caching on either side), so this measures the whole parse → plan → execute path.
//! Build with `--features sqlite` to include the SQLite columns.
//!
//! Run: `cargo run -p ferrodb-bench --release --features sqlite`

use std::time::Instant;

use engine::{Database, Output};

const ROWS: usize = 20_000;
const LOOKUPS: usize = 20_000;
const RANGE: usize = 200; // rows per range query
const RANGE_QUERIES: usize = 2_000;
/// Rows per batched multi-row INSERT. ferrodb's no-steal buffer pool bounds a
/// single transaction's dirty set, so bulk loads are batched (both engines).
const BATCH: usize = 250;

/// Build a batched `INSERT INTO <table> VALUES (i, gen(i)), …` for rows `[lo, hi)`.
fn insert_sql(table: &str, lo: usize, hi: usize, gen: fn(usize) -> i64) -> String {
    let mut sql = format!("INSERT INTO {table} VALUES ");
    for j in lo..hi {
        if j > lo {
            sql.push(',');
        }
        sql.push_str(&format!("({j},{})", gen(j)));
    }
    sql
}

fn gen_t(i: usize) -> i64 {
    (i * 7 % 1000) as i64
}
fn gen_u(i: usize) -> i64 {
    (i * 3 % 500) as i64
}

/// A tiny deterministic PRNG (SplitMix64-ish) so runs are reproducible.
fn next_rand(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// One benchmarked workload's timings.
struct Bench {
    name: String,
    ferrodb_ms: f64,
    sqlite_ms: Option<f64>,
}

fn main() {
    println!("ferrodb benchmarks — {ROWS} rows, in-memory, SQL-string driven\n");

    let ferro = run_ferrodb();
    #[cfg(feature = "sqlite")]
    let sqlite = run_sqlite();

    let mut rows: Vec<Bench> = Vec::new();
    for (i, (name, fms)) in ferro.iter().enumerate() {
        #[cfg(feature = "sqlite")]
        let sqlite_ms = Some(sqlite[i].1);
        #[cfg(not(feature = "sqlite"))]
        let sqlite_ms = {
            let _ = i;
            None
        };
        rows.push(Bench {
            name: name.clone(),
            ferrodb_ms: *fms,
            sqlite_ms,
        });
    }
    report(&rows);

    #[cfg(not(feature = "sqlite"))]
    println!("\n(build with --features sqlite for the SQLite comparison)");
}

// ---- ferrodb workloads ----------------------------------------------------

fn run_ferrodb() -> Vec<(String, f64)> {
    let mut out = Vec::new();

    // 1. Bulk insert, in batched multi-row statements. ferrodb's no-steal
    // buffer pool caps a single transaction's dirty set, so a 20k-row load must
    // be committed in batches; each batched INSERT autocommits (flushes) here.
    let mut db = Database::open_in_memory().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    let t = Instant::now();
    let mut i = 0;
    while i < ROWS {
        let end = (i + BATCH).min(ROWS);
        db.execute(&insert_sql("t", i, end, gen_t)).unwrap();
        i = end;
    }
    out.push((format!("bulk insert ({ROWS})"), ms(t)));

    // 2. Point lookups on the primary key (index seek).
    let mut seed = 0x1234_5678u64;
    let t = Instant::now();
    for _ in 0..LOOKUPS {
        let k = (next_rand(&mut seed) as usize % ROWS) as i64;
        let out = db
            .execute(&format!("SELECT v FROM t WHERE id = {k}"))
            .unwrap();
        black_box_rows(&out);
    }
    out.push((format!("point lookup ({LOOKUPS})"), ms(t)));

    // 3. Range scans.
    let mut seed = 0xABCD_EF01u64;
    let t = Instant::now();
    for _ in 0..RANGE_QUERIES {
        let a = next_rand(&mut seed) as usize % (ROWS - RANGE);
        let out = db
            .execute(&format!(
                "SELECT COUNT(*) FROM t WHERE id >= {a} AND id < {}",
                a + RANGE
            ))
            .unwrap();
        black_box_rows(&out);
    }
    out.push((format!("range scan ({RANGE_QUERIES}×{RANGE})"), ms(t)));

    // 4. Full-table aggregate. The `WHERE v >= 0` matches every row but forces
    // a real scan on both engines (SQLite special-cases a bare COUNT(*), which
    // would measure its metadata shortcut rather than scan+aggregate throughput).
    let t = Instant::now();
    for _ in 0..50 {
        let out = db
            .execute("SELECT COUNT(*), SUM(v), AVG(v) FROM t WHERE v >= 0")
            .unwrap();
        black_box_rows(&out);
    }
    out.push(("aggregate scan (50×)".to_string(), ms(t)));

    // 5. Hash join of two 20k tables.
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, w INTEGER)")
        .unwrap();
    let mut i = 0;
    while i < ROWS {
        let end = (i + BATCH).min(ROWS);
        db.execute(&insert_sql("u", i, end, gen_u)).unwrap();
        i = end;
    }
    let t = Instant::now();
    for _ in 0..10 {
        let out = db
            .execute("SELECT COUNT(*) FROM t JOIN u ON t.id = u.id")
            .unwrap();
        black_box_rows(&out);
    }
    out.push(("hash join (10×)".to_string(), ms(t)));

    out
}

fn black_box_rows(out: &Output) {
    if let Output::Rows { rows, .. } = out {
        std::hint::black_box(rows.len());
    }
}

// ---- SQLite workloads (mirrors the above) ---------------------------------

#[cfg(feature = "sqlite")]
fn run_sqlite() -> Vec<(String, f64)> {
    use rusqlite::Connection;
    let conn = Connection::open_in_memory().unwrap();
    let mut out = Vec::new();

    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)", [])
        .unwrap();
    // Same batched multi-row load as ferrodb, for an apples-to-apples comparison.
    let t = Instant::now();
    let mut i = 0;
    while i < ROWS {
        let end = (i + BATCH).min(ROWS);
        conn.execute(&insert_sql("t", i, end, gen_t), []).unwrap();
        i = end;
    }
    out.push((format!("bulk insert ({ROWS})"), ms(t)));

    let mut seed = 0x1234_5678u64;
    let t = Instant::now();
    for _ in 0..LOOKUPS {
        let k = (next_rand(&mut seed) as usize % ROWS) as i64;
        let _: i64 = conn
            .query_row(&format!("SELECT v FROM t WHERE id = {k}"), [], |r| r.get(0))
            .unwrap_or(0);
    }
    out.push((format!("point lookup ({LOOKUPS})"), ms(t)));

    let mut seed = 0xABCD_EF01u64;
    let t = Instant::now();
    for _ in 0..RANGE_QUERIES {
        let a = next_rand(&mut seed) as usize % (ROWS - RANGE);
        let _: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM t WHERE id >= {a} AND id < {}",
                    a + RANGE
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
    }
    out.push((format!("range scan ({RANGE_QUERIES}×{RANGE})"), ms(t)));

    let t = Instant::now();
    for _ in 0..50 {
        // Same three aggregates and full scan as the ferrodb side.
        let _: (i64, i64, f64) = conn
            .query_row(
                "SELECT COUNT(*), SUM(v), AVG(v) FROM t WHERE v >= 0",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
    }
    out.push(("aggregate scan (50×)".to_string(), ms(t)));

    conn.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, w INTEGER)", [])
        .unwrap();
    let mut i = 0;
    while i < ROWS {
        let end = (i + BATCH).min(ROWS);
        conn.execute(&insert_sql("u", i, end, gen_u), []).unwrap();
        i = end;
    }
    let t = Instant::now();
    for _ in 0..10 {
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM t JOIN u ON t.id = u.id", [], |r| {
                r.get(0)
            })
            .unwrap();
    }
    out.push(("hash join (10×)".to_string(), ms(t)));

    out
}

// ---- reporting ------------------------------------------------------------

fn report(rows: &[Bench]) {
    let has_sqlite = rows.iter().any(|r| r.sqlite_ms.is_some());
    if has_sqlite {
        println!(
            "{:<26} {:>12} {:>12} {:>10}",
            "workload", "ferrodb", "sqlite", "ratio"
        );
        println!("{}", "-".repeat(62));
        for r in rows {
            let s = r.sqlite_ms.unwrap();
            println!(
                "{:<26} {:>10.1}ms {:>10.1}ms {:>9.2}x",
                r.name,
                r.ferrodb_ms,
                s,
                r.ferrodb_ms / s
            );
        }
        println!("\nratio = ferrodb / sqlite  (1.0x = parity, >1 = ferrodb slower)");
    } else {
        println!("{:<26} {:>12}", "workload", "ferrodb");
        println!("{}", "-".repeat(40));
        for r in rows {
            println!("{:<26} {:>10.1}ms", r.name, r.ferrodb_ms);
        }
    }
}
