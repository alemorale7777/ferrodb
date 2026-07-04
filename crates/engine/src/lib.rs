//! ferrodb SQL engine with MVCC transactions.
//!
//! Each row key maps to a version chain ([`mvcc`]); transactions ([`txn`]) get a
//! snapshot at `begin` and see a consistent view. Statements run inside a
//! transaction (explicit `begin`/`commit_txn`/`rollback_txn`, or autocommit via
//! [`Database::execute`]). Durability rides on the M3 WAL.

pub mod catalog;
pub mod eval;
pub mod mvcc;
pub mod tuple;
pub mod txn;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

use sql::ast::{DataType, Expr, SelectItem, Statement, Value};
use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::meta::MetaPage;
use storage::page::PageId;
use storage::wal::Wal;

use catalog::{ColumnInfo, TableSchema};
use mvcc::Version;
use txn::{Status, TxnManager};

/// A row key paired with its decoded version chain.
type KeyedChain = (Vec<u8>, Vec<Version>);

use thiserror::Error;

// Re-exported so consumers can render results without depending on `sql` directly.
pub use sql::ast::Value as SqlValue;
pub use txn::TxnId;

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
    #[error("write conflict: {0}")]
    Conflict(String),
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

/// A single-file, MVCC, crash-safe SQL database.
pub struct Database {
    bp: BufferPool,
    meta: MetaPage,
    wal: Wal,
    mgr: TxnManager,
    write_sets: HashMap<TxnId, Vec<(String, Vec<u8>)>>,
}

impl Database {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Database, EngineError> {
        let data_path = path.as_ref();
        let mut wal_name = data_path.as_os_str().to_owned();
        wal_name.push(".wal");
        let wal_path = PathBuf::from(wal_name);

        let mut dm = DiskManager::open(data_path)?;
        let mut wal = Wal::open(&wal_path)?;
        wal.recover(&mut dm)?;

        let mut bp = BufferPool::new(dm, 256);
        bp.set_no_steal(true);
        let meta = load_meta(&mut bp)?;
        let mut db = Database {
            bp,
            meta,
            wal,
            mgr: TxnManager::new(1),
            write_sets: HashMap::new(),
        };
        // Resume transaction ids past anything already stored (derived, not persisted).
        let next = db.max_txn_id()? + 1;
        db.mgr = TxnManager::new(next);
        Ok(db)
    }

    // ---- transaction control ---------------------------------------------

    /// Begin a transaction and return its id.
    pub fn begin(&mut self) -> TxnId {
        let id = self.mgr.begin();
        self.write_sets.insert(id, Vec::new());
        id
    }

    /// Run a statement inside transaction `txn` (no commit).
    pub fn execute_in(&mut self, txn: TxnId, sql: &str) -> Result<Output, EngineError> {
        let stmt = sql::parse(sql)?;
        self.dispatch(txn, stmt)
    }

    /// Commit `txn`: freeze its hint bits, then durably persist (WAL + force).
    pub fn commit_txn(&mut self, txn: TxnId) -> Result<(), EngineError> {
        self.finalize_hints(txn)?;
        self.mgr.commit(txn);
        if self.bp.has_dirty() {
            self.wal_commit(txn)?;
            self.force_data()?;
        }
        self.write_sets.remove(&txn);
        Ok(())
    }

    /// Roll back `txn`: mark it aborted. Its versions are invisible; no undo needed.
    pub fn rollback_txn(&mut self, txn: TxnId) -> Result<(), EngineError> {
        self.mgr.abort(txn);
        self.write_sets.remove(&txn);
        Ok(())
    }

    /// Run a single statement as its own autocommit transaction.
    pub fn execute(&mut self, sql: &str) -> Result<Output, EngineError> {
        let txn = self.begin();
        match self.execute_in(txn, sql) {
            Ok(out) => {
                self.commit_txn(txn)?;
                Ok(out)
            }
            Err(e) => {
                self.rollback_txn(txn)?;
                Err(e)
            }
        }
    }

    /// Reclaim versions that are dead to every live snapshot.
    pub fn vacuum(&mut self) -> Result<Output, EngineError> {
        let horizon = self.mgr.oldest_active();
        let mut removed = 0usize;
        for table in catalog::list_tables(&mut self.bp, &self.meta)? {
            let mut schema = self.schema_of(&table)?;
            for (key, chain) in self.table_scan(schema.root)? {
                let before = chain.len();
                let kept: Vec<Version> = chain
                    .into_iter()
                    .filter(|v| !self.is_dead(v, horizon))
                    .collect();
                if kept.len() != before {
                    removed += before - kept.len();
                    self.table_put_chain(&mut schema, &key, &kept)?;
                }
            }
        }
        if self.bp.has_dirty() {
            let txn = self.mgr.begin();
            self.mgr.commit(txn);
            self.wal_commit(txn)?;
            self.force_data()?;
        }
        Ok(Output::Affected(removed))
    }

    fn is_dead(&self, v: &Version, horizon: TxnId) -> bool {
        // created by an aborted txn, or deleted-committed before any live snapshot
        matches!(self.mgr.known_status(v.xmin), Some(Status::Aborted))
            || (v.xmax != 0
                && mvcc::is_live_delete(&self.mgr, v.xmax, v.xmax_committed)
                && !self.mgr.is_active(v.xmax)
                && v.xmax < horizon)
    }

    /// Names of all user tables.
    pub fn list_tables(&mut self) -> Result<Vec<String>, EngineError> {
        catalog::list_tables(&mut self.bp, &self.meta)
    }

    /// Flush all pages and persist the meta record.
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        self.bp.flush_all()?;
        let mut mp = self.meta.encode();
        self.bp.disk_mut().write_page(PageId(0), &mut mp)?;
        self.bp.disk_mut().sync()?;
        Ok(())
    }

    // ---- durability (WAL) -------------------------------------------------

    fn wal_commit(&mut self, txn: TxnId) -> Result<(), EngineError> {
        for (pid, page) in self.bp.dirty_frames() {
            self.wal.append_update(txn, pid, &page)?;
        }
        let meta_page = self.meta.encode();
        self.wal.append_update(txn, PageId(0), &meta_page)?;
        self.wal.append_commit(txn)?;
        self.wal.sync()?;
        Ok(())
    }

    fn force_data(&mut self) -> Result<(), EngineError> {
        self.bp.flush_all()?;
        let mut mp = self.meta.encode();
        self.bp.disk_mut().write_page(PageId(0), &mut mp)?;
        self.bp.disk_mut().sync()?;
        self.wal.reset()?;
        Ok(())
    }

    /// Set the committed hint bits for every version this transaction touched.
    fn finalize_hints(&mut self, txn: TxnId) -> Result<(), EngineError> {
        let ws = self.write_sets.get(&txn).cloned().unwrap_or_default();
        for (table, key) in ws {
            let mut schema = match catalog::get_table(&mut self.bp, &self.meta, &table)? {
                Some(s) => s,
                None => continue,
            };
            if let Some(chain) = self.table_get_chain(schema.root, &key)? {
                let mut chain = chain;
                let mut changed = false;
                for v in &mut chain {
                    if v.xmin == txn && !v.xmin_committed {
                        v.xmin_committed = true;
                        changed = true;
                    }
                    if v.xmax == txn && !v.xmax_committed {
                        v.xmax_committed = true;
                        changed = true;
                    }
                }
                if changed {
                    self.table_put_chain(&mut schema, &key, &chain)?;
                }
            }
        }
        Ok(())
    }

    fn max_txn_id(&mut self) -> Result<TxnId, EngineError> {
        let mut max = 0u64;
        for table in catalog::list_tables(&mut self.bp, &self.meta)? {
            let schema = self.schema_of(&table)?;
            for (_k, chain) in self.table_scan(schema.root)? {
                for v in chain {
                    max = max.max(v.xmin).max(v.xmax);
                }
            }
        }
        Ok(max)
    }

    // ---- chain storage helpers -------------------------------------------

    fn table_get_chain(
        &mut self,
        root: PageId,
        key: &[u8],
    ) -> Result<Option<Vec<Version>>, EngineError> {
        let raw = {
            let mut tree = BPlusTree::open_at(&mut self.bp, root);
            tree.get(key)?
        };
        Ok(raw.map(|b| mvcc::decode_chain(&b)))
    }

    fn table_put_chain(
        &mut self,
        schema: &mut TableSchema,
        key: &[u8],
        chain: &[Version],
    ) -> Result<(), EngineError> {
        let bytes = mvcc::encode_chain(chain);
        {
            let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
            tree.insert(key, &bytes)?;
            schema.root = tree.root();
        }
        catalog::put_table(&mut self.bp, &mut self.meta, schema)?;
        Ok(())
    }

    fn table_scan(&mut self, root: PageId) -> Result<Vec<KeyedChain>, EngineError> {
        let raw = {
            let mut tree = BPlusTree::open_at(&mut self.bp, root);
            tree.scan(None, None)?
        };
        Ok(raw
            .into_iter()
            .map(|(k, b)| (k, mvcc::decode_chain(&b)))
            .collect())
    }

    fn schema_of(&mut self, name: &str) -> Result<TableSchema, EngineError> {
        catalog::get_table(&mut self.bp, &self.meta, name)?
            .ok_or_else(|| EngineError::UnknownTable(name.to_string()))
    }

    // ---- dispatch ---------------------------------------------------------

    fn dispatch(&mut self, txn: TxnId, stmt: Statement) -> Result<Output, EngineError> {
        match stmt {
            Statement::CreateTable { name, columns } => self.exec_create(name, columns),
            Statement::DropTable { name } => self.exec_drop(name),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.exec_insert(txn, table, columns, rows),
            Statement::Select {
                items,
                from,
                filter,
                order_by,
                limit,
                offset,
            } => self.exec_select(txn, items, from, filter, order_by, limit, offset),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.exec_update(txn, table, assignments, filter),
            Statement::Delete { table, filter } => self.exec_delete(txn, table, filter),
        }
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
        if catalog::drop_table(&mut self.bp, &mut self.meta, &name)? {
            Ok(Output::Ack("DROP TABLE"))
        } else {
            Err(EngineError::UnknownTable(name))
        }
    }

    // ---- INSERT -----------------------------------------------------------

    fn exec_insert(
        &mut self,
        txn: TxnId,
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    ) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let snap = self.mgr.snapshot(txn).clone();
        let ncols = schema.columns.len();
        let empty = empty_schema();
        let mut count = 0;

        for exprs in rows {
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
                        row[idx] = coerce(eval::eval(e, &empty, &[])?, &schema.columns[idx])?;
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
                        row[i] = coerce(eval::eval(e, &empty, &[])?, &schema.columns[i])?;
                        provided[i] = true;
                    }
                }
            }
            for (i, c) in schema.columns.iter().enumerate() {
                if !provided[i] && c.not_null {
                    return Err(EngineError::Constraint(format!(
                        "column '{}' is NOT NULL but was not provided",
                        c.name
                    )));
                }
            }

            let key = match schema.pk_index() {
                Some(pk) => tuple::value_to_key(&row[pk])?,
                None => {
                    let k = tuple::rowid_key(schema.next_rowid);
                    schema.next_rowid += 1;
                    k
                }
            };
            let mut chain = self.table_get_chain(schema.root, &key)?.unwrap_or_default();
            if schema.pk_index().is_some()
                && mvcc::visible_index(&chain, &snap, &self.mgr).is_some()
            {
                return Err(EngineError::Constraint("duplicate primary key".into()));
            }
            chain.push(Version::new(txn, tuple::encode_tuple(&row)));
            self.table_put_chain(&mut schema, &key, &chain)?;
            self.write_sets
                .get_mut(&txn)
                .expect("active txn")
                .push((table.clone(), key));
            count += 1;
        }
        Ok(Output::Affected(count))
    }

    // ---- SELECT -----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn exec_select(
        &mut self,
        txn: TxnId,
        items: Vec<SelectItem>,
        from: String,
        filter: Option<Expr>,
        order_by: Option<sql::ast::OrderBy>,
        limit: Option<u64>,
        offset: Option<u64>,
    ) -> Result<Output, EngineError> {
        let schema = self.schema_of(&from)?;
        let snap = self.mgr.snapshot(txn).clone();
        let types = schema.types();

        let mut rows: Vec<Vec<Value>> = Vec::new();
        for (_key, chain) in self.table_scan(schema.root)? {
            if let Some(i) = mvcc::visible_index(&chain, &snap, &self.mgr) {
                rows.push(tuple::decode_tuple(&types, &chain[i].data)?);
            }
        }

        if let Some(pred) = &filter {
            let mut kept = Vec::new();
            for row in rows {
                if matches!(eval::eval(pred, &schema, &row)?, Value::Boolean(true)) {
                    kept.push(row);
                }
            }
            rows = kept;
        }

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

        let indices = resolve_projection(&items, &schema)?;
        let columns = indices
            .iter()
            .map(|&i| schema.columns[i].name.clone())
            .collect();
        let projected = rows
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
        txn: TxnId,
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    ) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let snap = self.mgr.snapshot(txn).clone();
        let types = schema.types();
        if let Some(pk) = schema.pk_index() {
            if assignments
                .iter()
                .any(|(c, _)| schema.column_index(c) == Some(pk))
            {
                return Err(EngineError::Unsupported("updating the primary key".into()));
            }
        }

        let mut ops: Vec<KeyedChain> = Vec::new();
        for (key, chain) in self.table_scan(schema.root)? {
            let Some(i) = mvcc::visible_index(&chain, &snap, &self.mgr) else {
                continue;
            };
            let row = tuple::decode_tuple(&types, &chain[i].data)?;
            let keep = match &filter {
                Some(p) => matches!(eval::eval(p, &schema, &row)?, Value::Boolean(true)),
                None => true,
            };
            if !keep {
                continue;
            }
            let v = &chain[i];
            if v.xmax != txn && mvcc::is_live_delete(&self.mgr, v.xmax, v.xmax_committed) {
                return Err(EngineError::Conflict(
                    "row was updated by a concurrent transaction".into(),
                ));
            }
            let mut newrow = row.clone();
            for (cname, e) in &assignments {
                let idx = schema
                    .column_index(cname)
                    .ok_or_else(|| EngineError::UnknownColumn(cname.clone()))?;
                newrow[idx] = coerce(eval::eval(e, &schema, &row)?, &schema.columns[idx])?;
            }
            let mut newchain = chain.clone();
            newchain[i].xmax = txn;
            newchain.push(Version::new(txn, tuple::encode_tuple(&newrow)));
            ops.push((key, newchain));
        }

        let count = ops.len();
        for (key, chain) in ops {
            self.table_put_chain(&mut schema, &key, &chain)?;
            self.write_sets
                .get_mut(&txn)
                .expect("active txn")
                .push((table.clone(), key));
        }
        Ok(Output::Affected(count))
    }

    fn exec_delete(
        &mut self,
        txn: TxnId,
        table: String,
        filter: Option<Expr>,
    ) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let snap = self.mgr.snapshot(txn).clone();
        let types = schema.types();

        let mut ops: Vec<KeyedChain> = Vec::new();
        for (key, chain) in self.table_scan(schema.root)? {
            let Some(i) = mvcc::visible_index(&chain, &snap, &self.mgr) else {
                continue;
            };
            let row = tuple::decode_tuple(&types, &chain[i].data)?;
            let del = match &filter {
                Some(p) => matches!(eval::eval(p, &schema, &row)?, Value::Boolean(true)),
                None => true,
            };
            if !del {
                continue;
            }
            let v = &chain[i];
            if v.xmax != txn && mvcc::is_live_delete(&self.mgr, v.xmax, v.xmax_committed) {
                return Err(EngineError::Conflict(
                    "row was deleted by a concurrent transaction".into(),
                ));
            }
            let mut newchain = chain.clone();
            newchain[i].xmax = txn;
            ops.push((key, newchain));
        }

        let count = ops.len();
        for (key, chain) in ops {
            self.table_put_chain(&mut schema, &key, &chain)?;
            self.write_sets
                .get_mut(&txn)
                .expect("active txn")
                .push((table.clone(), key));
        }
        Ok(Output::Affected(count))
    }
}

// ---- helpers --------------------------------------------------------------

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
    //! Deterministic crash simulations that drive WAL recovery directly.
    use super::*;

    fn tmp_path() -> std::path::PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crash.db");
        std::mem::forget(dir);
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
            // Freeze hints + WAL-commit, but crash before force_data().
            let txn = db.begin();
            db.execute_in(txn, "INSERT INTO t VALUES (2, 20)").unwrap();
            db.finalize_hints(txn).unwrap();
            db.mgr.commit(txn);
            db.wal_commit(txn).unwrap();
        }
        {
            let mut db = Database::open(&path).unwrap();
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
            let txn = db.begin();
            db.execute_in(txn, "INSERT INTO t VALUES (2, 20)").unwrap();
            // crash: drop without finalize/commit
        }
        {
            let mut db = Database::open(&path).unwrap();
            assert_eq!(select_vs(&mut db), vec![10]);
        }
    }
}
