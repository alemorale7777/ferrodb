//! ferrodb SQL engine: parse → resolve → execute SQL against the M1 storage
//! engine. Single-table volcano-style execution; transactions/joins/optimizer
//! arrive in later milestones.

pub mod catalog;
pub mod eval;
pub mod tuple;

use std::cmp::Ordering;
use std::path::PathBuf;

use sql::ast::{DataType, Expr, SelectItem, Statement, Value};

// Re-exported so consumers can render results without depending on `sql` directly.
pub use sql::ast::Value as SqlValue;
use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::meta::MetaPage;
use storage::page::PageId;
use storage::wal::Wal;

use catalog::{ColumnInfo, TableSchema};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Storage(#[from] storage::StorageError),
    #[error(transparent)]
    Sql(#[from] sql::SqlError),
    #[error("unknown table '{0}'")]
    UnknownTable(String),
    #[error("table '{0}' already exists")]
    TableExists(String),
    #[error("unknown column '{0}'")]
    UnknownColumn(String),
    #[error("type error: {0}")]
    Type(String),
    #[error("constraint violation: {0}")]
    Constraint(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

/// The result of executing one statement.
#[derive(Debug, PartialEq)]
pub enum Output {
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Affected(usize),
    Ack(&'static str),
}

/// A single-file SQL database with write-ahead logging for crash recovery.
pub struct Database {
    bp: BufferPool,
    meta: MetaPage,
    wal: Wal,
    next_txn: u64,
}

impl Database {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Database, EngineError> {
        let data_path = path.as_ref();
        let mut wal_name = data_path.as_os_str().to_owned();
        wal_name.push(".wal");
        let wal_path = PathBuf::from(wal_name);

        // Recover any committed-but-unflushed work before reading the catalog.
        let mut dm = DiskManager::open(data_path)?;
        let mut wal = Wal::open(&wal_path)?;
        wal.recover(&mut dm)?;

        let mut bp = BufferPool::new(dm, 256);
        bp.set_no_steal(true); // WAL recovery requires no-steal
        let meta = load_meta(&mut bp)?;
        Ok(Database {
            bp,
            meta,
            wal,
            next_txn: 1,
        })
    }

    /// Flush all pages and persist the meta record. Call before dropping.
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        self.bp.flush_all()?;
        let mut mp = self.meta.encode();
        self.bp.disk_mut().write_page(PageId(0), &mut mp)?;
        self.bp.disk_mut().sync()?;
        Ok(())
    }

    /// Parse and execute a single SQL statement as its own autocommit transaction.
    /// On success the statement is durable (WAL + data flushed); on error it is
    /// rolled back entirely, leaving the database at its prior committed state.
    pub fn execute(&mut self, sql: &str) -> Result<Output, EngineError> {
        let stmt = sql::parse(sql)?;
        let saved = self.meta; // MetaPage: Copy — statement-start snapshot for rollback
        let txn = self.next_txn;
        self.next_txn += 1;
        match self.dispatch(stmt) {
            Ok(out) => {
                // Read-only statements dirty nothing — skip the commit fsync.
                if self.bp.has_dirty() {
                    self.commit(txn)?;
                }
                Ok(out)
            }
            Err(e) => {
                self.meta = saved;
                self.bp.discard_dirty()?;
                Err(e)
            }
        }
    }

    fn commit(&mut self, txn: u64) -> Result<(), EngineError> {
        self.wal_commit(txn)?;
        self.force_data()
    }

    /// Log the transaction's dirty pages + meta, then a commit record, then fsync.
    fn wal_commit(&mut self, txn: u64) -> Result<(), EngineError> {
        for (pid, page) in self.bp.dirty_frames() {
            self.wal.append_update(txn, pid, &page)?;
        }
        let meta_page = self.meta.encode();
        self.wal.append_update(txn, PageId(0), &meta_page)?;
        self.wal.append_commit(txn)?;
        self.wal.sync()?;
        Ok(())
    }

    /// Force dirty data pages + meta to the data file, then clear the WAL.
    fn force_data(&mut self) -> Result<(), EngineError> {
        self.bp.flush_all()?;
        let mut mp = self.meta.encode();
        self.bp.disk_mut().write_page(PageId(0), &mut mp)?;
        self.bp.disk_mut().sync()?;
        self.wal.reset()?;
        Ok(())
    }

    /// Execute a parsed statement, mutating in-memory state without committing.
    fn dispatch(&mut self, stmt: Statement) -> Result<Output, EngineError> {
        match stmt {
            Statement::CreateTable { name, columns } => self.exec_create(name, columns),
            Statement::DropTable { name } => self.exec_drop(name),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.exec_insert(table, columns, rows),
            Statement::Select {
                items,
                from,
                filter,
                order_by,
                limit,
                offset,
            } => self.exec_select(items, from, filter, order_by, limit, offset),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.exec_update(table, assignments, filter),
            Statement::Delete { table, filter } => self.exec_delete(table, filter),
        }
    }

    /// Names of all user tables.
    pub fn list_tables(&mut self) -> Result<Vec<String>, EngineError> {
        catalog::list_tables(&mut self.bp, &self.meta)
    }

    fn schema_of(&mut self, name: &str) -> Result<TableSchema, EngineError> {
        catalog::get_table(&mut self.bp, &self.meta, name)?
            .ok_or_else(|| EngineError::UnknownTable(name.to_string()))
    }

    // ---- DDL --------------------------------------------------------------

    fn exec_create(
        &mut self,
        name: String,
        columns: Vec<sql::ast::ColumnDef>,
    ) -> Result<Output, EngineError> {
        if catalog::get_table(&mut self.bp, &self.meta, &name)?.is_some() {
            return Err(EngineError::TableExists(name));
        }
        if columns.iter().filter(|c| c.primary_key).count() > 1 {
            return Err(EngineError::Unsupported("multiple primary keys".into()));
        }
        let cols = columns
            .into_iter()
            .map(|c| ColumnInfo {
                name: c.name,
                data_type: c.data_type,
                not_null: c.not_null,
                primary_key: c.primary_key,
            })
            .collect();
        let data_root = BPlusTree::create(&mut self.bp)?.root();
        let schema = TableSchema {
            name,
            columns: cols,
            root: data_root,
            next_rowid: 1,
        };
        catalog::put_table(&mut self.bp, &mut self.meta, &schema)?;
        Ok(Output::Ack("CREATE TABLE"))
    }

    fn exec_drop(&mut self, name: String) -> Result<Output, EngineError> {
        // NB: M2 leaks the table's data pages; page reclamation lands with M4 VACUUM.
        if catalog::drop_table(&mut self.bp, &mut self.meta, &name)? {
            Ok(Output::Ack("DROP TABLE"))
        } else {
            Err(EngineError::UnknownTable(name))
        }
    }

    // ---- INSERT -----------------------------------------------------------

    fn exec_insert(
        &mut self,
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    ) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let ncols = schema.columns.len();
        let empty = empty_schema();
        let mut count = 0;

        for exprs in rows {
            // build the full row in table-column order
            let mut row: Vec<Value> = vec![Value::Null; ncols];
            let mut provided = vec![false; ncols];
            match &columns {
                Some(cols) => {
                    if cols.len() != exprs.len() {
                        return Err(EngineError::Other(
                            "column count does not match value count".into(),
                        ));
                    }
                    for (cname, e) in cols.iter().zip(exprs.iter()) {
                        let idx = schema
                            .column_index(cname)
                            .ok_or_else(|| EngineError::UnknownColumn(cname.clone()))?;
                        let v = eval::eval(e, &empty, &[])?;
                        row[idx] = coerce(v, &schema.columns[idx])?;
                        provided[idx] = true;
                    }
                }
                None => {
                    if exprs.len() != ncols {
                        return Err(EngineError::Other(
                            "value count does not match column count".into(),
                        ));
                    }
                    for (i, e) in exprs.iter().enumerate() {
                        let v = eval::eval(e, &empty, &[])?;
                        row[i] = coerce(v, &schema.columns[i])?;
                        provided[i] = true;
                    }
                }
            }
            // NOT NULL check for columns left unset
            for (i, c) in schema.columns.iter().enumerate() {
                if !provided[i] && c.not_null {
                    return Err(EngineError::Constraint(format!(
                        "column '{}' is NOT NULL but was not provided",
                        c.name
                    )));
                }
            }

            // key: primary key value, or a hidden auto-increment row id
            let key = match schema.pk_index() {
                Some(pk) => tuple::value_to_key(&row[pk])?,
                None => {
                    let k = tuple::rowid_key(schema.next_rowid);
                    schema.next_rowid += 1;
                    k
                }
            };
            let encoded = tuple::encode_tuple(&row);
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            tree.insert(&key, &encoded)?;
            schema.root = tree.root();
            count += 1;
        }

        catalog::put_table(&mut self.bp, &mut self.meta, &schema)?;
        Ok(Output::Affected(count))
    }

    // ---- SELECT -----------------------------------------------------------

    fn exec_select(
        &mut self,
        items: Vec<SelectItem>,
        from: String,
        filter: Option<Expr>,
        order_by: Option<sql::ast::OrderBy>,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Result<Output, EngineError> {
        let schema = self.schema_of(&from)?;
        let types = schema.types();

        // SeqScan → decode
        let mut rows: Vec<Vec<Value>> = {
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            let raw = tree.scan(None, None)?;
            let mut out = Vec::with_capacity(raw.len());
            for (_k, bytes) in raw {
                out.push(tuple::decode_tuple(&types, &bytes)?);
            }
            out
        };

        // Filter
        if let Some(pred) = &filter {
            let mut kept = Vec::new();
            for row in rows {
                if matches!(eval::eval(pred, &schema, &row)?, Value::Boolean(true)) {
                    kept.push(row);
                }
            }
            rows = kept;
        }

        // Order by
        if let Some(ob) = &order_by {
            let idx = schema
                .column_index(&ob.column)
                .ok_or_else(|| EngineError::UnknownColumn(ob.column.clone()))?;
            rows.sort_by(|a, b| {
                let ord = value_cmp(&a[idx], &b[idx]);
                if ob.descending {
                    ord.reverse()
                } else {
                    ord
                }
            });
        }

        // Offset / Limit
        if let Some(off) = offset {
            let off = off as usize;
            rows = if off >= rows.len() {
                Vec::new()
            } else {
                rows.split_off(off)
            };
        }
        if let Some(lim) = limit {
            rows.truncate(lim as usize);
        }

        // Projection
        let indices: Vec<usize> = resolve_projection(&items, &schema)?;
        let columns: Vec<String> = indices
            .iter()
            .map(|&i| schema.columns[i].name.clone())
            .collect();
        let projected: Vec<Vec<Value>> = rows
            .into_iter()
            .map(|row| indices.iter().map(|&i| row[i].clone()).collect())
            .collect();

        Ok(Output::Rows {
            columns,
            rows: projected,
        })
    }

    // ---- UPDATE / DELETE --------------------------------------------------

    fn exec_update(
        &mut self,
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    ) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let types = schema.types();
        if let Some(pk) = schema.pk_index() {
            if assignments
                .iter()
                .any(|(c, _)| schema.column_index(c) == Some(pk))
            {
                return Err(EngineError::Unsupported("updating the primary key".into()));
            }
        }

        // gather (key, new_tuple) for matching rows, then apply
        let mut updates: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        {
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            for (key, bytes) in tree.scan(None, None)? {
                let mut row = tuple::decode_tuple(&types, &bytes)?;
                let keep = match &filter {
                    Some(p) => matches!(eval::eval(p, &schema, &row)?, Value::Boolean(true)),
                    None => true,
                };
                if !keep {
                    continue;
                }
                for (cname, e) in &assignments {
                    let idx = schema
                        .column_index(cname)
                        .ok_or_else(|| EngineError::UnknownColumn(cname.clone()))?;
                    let v = eval::eval(e, &schema, &row)?;
                    row[idx] = coerce(v, &schema.columns[idx])?;
                }
                updates.push((key, tuple::encode_tuple(&row)));
            }
        }
        let count = updates.len();
        {
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            for (key, bytes) in updates {
                tree.insert(&key, &bytes)?; // same key → overwrite
            }
            schema.root = tree.root();
        }
        catalog::put_table(&mut self.bp, &mut self.meta, &schema)?;
        Ok(Output::Affected(count))
    }

    fn exec_delete(&mut self, table: String, filter: Option<Expr>) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let types = schema.types();

        let mut keys: Vec<Vec<u8>> = Vec::new();
        {
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            for (key, bytes) in tree.scan(None, None)? {
                let row = tuple::decode_tuple(&types, &bytes)?;
                let del = match &filter {
                    Some(p) => matches!(eval::eval(p, &schema, &row)?, Value::Boolean(true)),
                    None => true,
                };
                if del {
                    keys.push(key);
                }
            }
        }
        let count = keys.len();
        {
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            for key in keys {
                tree.delete(&key)?;
            }
            schema.root = tree.root();
        }
        catalog::put_table(&mut self.bp, &mut self.meta, &schema)?;
        Ok(Output::Affected(count))
    }
}

// ---- helpers --------------------------------------------------------------

/// An empty schema for evaluating column-free expressions (e.g. `VALUES`).
fn empty_schema() -> TableSchema {
    TableSchema {
        name: String::new(),
        columns: Vec::new(),
        root: PageId(0),
        next_rowid: 0,
    }
}

fn resolve_projection(
    items: &[SelectItem],
    schema: &TableSchema,
) -> Result<Vec<usize>, EngineError> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => out.extend(0..schema.columns.len()),
            SelectItem::Column(name) => out.push(
                schema
                    .column_index(name)
                    .ok_or_else(|| EngineError::UnknownColumn(name.clone()))?,
            ),
        }
    }
    Ok(out)
}

fn coerce(v: Value, col: &ColumnInfo) -> Result<Value, EngineError> {
    let null_ok = |col: &ColumnInfo| {
        if col.not_null {
            Err(EngineError::Constraint(format!(
                "column '{}' is NOT NULL",
                col.name
            )))
        } else {
            Ok(Value::Null)
        }
    };
    match col.data_type {
        DataType::Integer => match v {
            Value::Null => null_ok(col),
            Value::Integer(_) => Ok(v),
            other => Err(EngineError::Type(format!(
                "column '{}' expects INTEGER, got {other:?}",
                col.name
            ))),
        },
        DataType::Real => match v {
            Value::Null => null_ok(col),
            Value::Real(_) => Ok(v),
            Value::Integer(x) => Ok(Value::Real(x as f64)),
            other => Err(EngineError::Type(format!(
                "column '{}' expects REAL, got {other:?}",
                col.name
            ))),
        },
        DataType::Text => match v {
            Value::Null => null_ok(col),
            Value::Text(_) => Ok(v),
            other => Err(EngineError::Type(format!(
                "column '{}' expects TEXT, got {other:?}",
                col.name
            ))),
        },
        DataType::Boolean => match v {
            Value::Null => null_ok(col),
            Value::Boolean(_) => Ok(v),
            other => Err(EngineError::Type(format!(
                "column '{}' expects BOOLEAN, got {other:?}",
                col.name
            ))),
        },
    }
}

/// Total order for `ORDER BY`; `NULL` sorts before all non-null values.
fn value_cmp(a: &Value, b: &Value) -> Ordering {
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

#[cfg(test)]
mod crash_tests {
    //! Deterministic crash simulations that drive the WAL recovery path directly,
    //! rather than racing an OS `kill` (reliable in CI).
    use super::*;

    fn tmp_path() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash.db");
        std::mem::forget(dir); // keep the file alive for the whole test
        p
    }

    fn select_vs(db: &mut Database) -> Vec<i64> {
        match db.execute("SELECT v FROM t ORDER BY id").unwrap() {
            Output::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Integer(x) => x,
                    _ => panic!(),
                })
                .collect(),
            _ => panic!(),
        }
    }

    #[test]
    fn redo_recovers_a_committed_txn_after_crash_before_data_flush() {
        let path = tmp_path();
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
                .unwrap();
            db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
            // Crash *after* the WAL commit is durable but *before* data is flushed:
            // log + commit to the WAL, then drop without force_data(). No-steal means
            // the dirty buffer pages never reached disk, so only the WAL has the change.
            let txn = db.next_txn;
            db.next_txn += 1;
            let stmt = sql::parse("INSERT INTO t VALUES (2, 20)").unwrap();
            db.dispatch(stmt).unwrap();
            db.wal_commit(txn).unwrap();
        }
        {
            let mut db = Database::open(&path).unwrap(); // recovery replays the WAL
            assert_eq!(select_vs(&mut db), vec![10, 20]);
        }
    }

    #[test]
    fn an_uncommitted_txn_leaves_no_trace_after_crash() {
        let path = tmp_path();
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
                .unwrap();
            db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
            // Mutate a statement but never write its commit to the WAL, then crash.
            let stmt = sql::parse("INSERT INTO t VALUES (2, 20)").unwrap();
            db.dispatch(stmt).unwrap();
        }
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(select_vs(&mut db), vec![10]); // row 2 is gone; row 1 intact
        }
    }
}
