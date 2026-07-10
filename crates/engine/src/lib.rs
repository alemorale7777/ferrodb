//! ferrodb SQL engine with MVCC transactions.
//!
//! Each row key maps to a version chain ([`mvcc`]); transactions ([`txn`]) get a
//! snapshot at `begin` and see a consistent view. Statements run inside a
//! transaction (explicit `begin`/`commit_txn`/`rollback_txn`, or autocommit via
//! [`Database::execute`]). Durability rides on the M3 WAL.

pub mod catalog;
pub mod eval;
pub mod mvcc;
pub mod plan;
pub mod planner;
pub mod treeview;
pub mod tuple;
pub mod txn;

use std::collections::HashMap;
use std::path::PathBuf;

use sql::ast::{BinOp, DataType, Expr, JoinType, Select, SelectItem, Statement, TableRef, Value};
use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::meta::MetaPage;
use storage::page::PageId;
use storage::wal::Wal;

use catalog::{ColumnInfo, IndexInfo, TableSchema};
use mvcc::Version;
use plan::{Access, Col, Plan, RowSet};
use txn::{Snapshot, Status, TxnManager};

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
    /// Data-file path; sidecar index files derive from it (None: in-memory).
    base_path: Option<PathBuf>,
    /// Live HNSW indexes, keyed `"table\0column"` (lowercased). Loaded from
    /// sidecars lazily; rebuilt from the base table when missing or stale —
    /// the index is derived data, the table is the source of truth.
    hnsw: HashMap<String, vector::hnsw::Hnsw>,
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
        Database::from_parts(dm, wal, Some(data_path.to_path_buf()))
    }

    /// Open a database backed entirely by memory — no filesystem, no persistence.
    /// This is what lets the engine run in the browser (WebAssembly).
    pub fn open_in_memory() -> Result<Database, EngineError> {
        Database::from_parts(DiskManager::in_memory(), Wal::in_memory(), None)
    }

    fn from_parts(
        dm: DiskManager,
        wal: Wal,
        base_path: Option<PathBuf>,
    ) -> Result<Database, EngineError> {
        let mut bp = BufferPool::new(dm, 256);
        bp.set_no_steal(true);
        let meta = load_meta(&mut bp)?;
        let mut db = Database {
            bp,
            meta,
            wal,
            mgr: TxnManager::new(1),
            write_sets: HashMap::new(),
            base_path,
            hnsw: HashMap::new(),
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

    /// A view of a table's B+-tree structure (for the web playground visualizer).
    pub fn table_tree(&mut self, table: &str) -> Result<treeview::TreeNode, EngineError> {
        let schema = self.schema_of(table)?;
        let kind =
            treeview::KeyKind::for_pk(schema.pk_index().map(|i| schema.columns[i].data_type));
        self.walk_tree(schema.root, kind)
    }

    /// A table's B+-tree structure as JSON.
    pub fn table_tree_json(&mut self, table: &str) -> Result<String, EngineError> {
        Ok(self.table_tree(table)?.to_json())
    }

    fn walk_tree(
        &mut self,
        pid: PageId,
        kind: treeview::KeyKind,
    ) -> Result<treeview::TreeNode, EngineError> {
        let f = self.bp.fetch(pid)?;
        let page = self.bp.frame(f).clone();
        self.bp.unpin(f);

        if treeview::is_leaf(&page) {
            return Ok(treeview::TreeNode::leaf(&page, kind));
        }
        let mut children = vec![self.walk_tree(storage::btree::node::left_child(&page), kind)?];
        let mut keys = Vec::new();
        for (k, child) in storage::btree::node::internal_entries(&page) {
            keys.push(kind.render(&k));
            children.push(self.walk_tree(child, kind)?);
        }
        Ok(treeview::TreeNode {
            leaf: false,
            keys,
            children,
        })
    }

    /// Flush all pages and persist the meta record (and index sidecars).
    pub fn checkpoint(&mut self) -> Result<(), EngineError> {
        self.save_indexes()?;
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

    // ---- vector indexes (M9) ----------------------------------------------
    //
    // The HNSW index is a *secondary* index: it maps vectors to row keys, and
    // rows are fetched through the primary B+-tree — the same division of
    // labor pgvector has with Postgres's heap. The index is not MVCC-aware:
    // it may return keys for rows a snapshot cannot see (aborted inserts,
    // concurrent writers); the B+-tree fetch applies `mvcc::visible_index`
    // and silently drops them, exactly as Postgres drops dead TIDs after a
    // heap fetch. Deletes tombstone the index node (it keeps routing, stops
    // being returned); a rebuild reclaims tombstones.

    fn index_map_key(table: &str, column: &str) -> String {
        format!(
            "{}\0{}",
            table.to_ascii_lowercase(),
            column.to_ascii_lowercase()
        )
    }

    /// `<data-file>.hnsw-<table>-<column>` next to the database file.
    fn sidecar_path(&self, table: &str, column: &str) -> Option<PathBuf> {
        let base = self.base_path.as_ref()?;
        let mut name = base.as_os_str().to_owned();
        name.push(format!(
            ".hnsw-{}-{}",
            table.to_ascii_lowercase(),
            column.to_ascii_lowercase()
        ));
        Some(PathBuf::from(name))
    }

    fn vector_dim(schema: &TableSchema, column: &str) -> Option<u16> {
        let i = schema.column_index(column)?;
        match schema.columns[i].data_type {
            DataType::Vector(d) => Some(d),
            _ => None,
        }
    }

    /// Build a fresh index over the newest version of every row key. Version
    /// chains share one key; we index the newest vector and let MVCC filter
    /// at fetch time (a snapshot that should see an older version still gets
    /// the row — ranked by the newest vector, a slight staleness we accept
    /// and document rather than versioning the index).
    fn build_hnsw(
        &mut self,
        schema: &TableSchema,
        ix: &IndexInfo,
        dim: u16,
    ) -> Result<vector::hnsw::Hnsw, EngineError> {
        let params = vector::hnsw::HnswParams {
            m: ix.m as usize,
            m_max0: 2 * ix.m as usize,
            ef_construction: ix.ef_construction as usize,
        };
        let mut idx = vector::hnsw::Hnsw::new(
            dim as usize,
            vector::distance::Metric::L2,
            params,
            0xFE44_0DB0,
        );
        let types = schema.types();
        let col = schema
            .column_index(&ix.column)
            .ok_or_else(|| EngineError::UnknownColumn(ix.column.clone()))?;
        for (key, chain) in self.table_scan(schema.root)? {
            let Some(v) = chain.last() else { continue };
            let row = tuple::decode_tuple(&types, &v.data)?;
            if let Value::Vector(vec) = &row[col] {
                idx.insert(vec, &key);
            }
        }
        Ok(idx)
    }

    /// Make sure the index for (`table`, `ix`) is resident: use the loaded
    /// one, else load the sidecar (rejecting a stale or torn file), else
    /// rebuild from the table. Rebuild-on-mismatch is what makes skipping
    /// per-commit sidecar writes safe: the sidecar is only a warm-start.
    fn ensure_hnsw(
        &mut self,
        table: &str,
        ix: &IndexInfo,
        dim: u16,
        schema: &TableSchema,
    ) -> Result<(), EngineError> {
        let key = Self::index_map_key(table, &ix.column);
        if self.hnsw.contains_key(&key) {
            return Ok(());
        }
        // Freshness bar: the index must hold exactly one node per row key
        // (minus nothing — tombstones stay as nodes). Chain count is that
        // upper bound; mismatch ⇒ the sidecar predates some insert ⇒ rebuild.
        let nkeys = self.table_scan(schema.root)?.len();
        if let Some(p) = self.sidecar_path(table, &ix.column) {
            if let Ok(loaded) = vector::hnsw::Hnsw::load(&p) {
                if loaded.len() == nkeys && loaded.dim() == dim as usize {
                    self.hnsw.insert(key, loaded);
                    return Ok(());
                }
            }
        }
        let built = self.build_hnsw(schema, ix, dim)?;
        self.hnsw.insert(key, built);
        Ok(())
    }

    /// Persist every resident index to its sidecar (no-op in memory).
    fn save_indexes(&mut self) -> Result<(), EngineError> {
        if self.base_path.is_none() {
            return Ok(());
        }
        let keys: Vec<String> = self.hnsw.keys().cloned().collect();
        for key in keys {
            let (table, column) = key.split_once('\0').expect("map key shape");
            if let Some(p) = self.sidecar_path(table, column) {
                self.hnsw[&key]
                    .save(&p)
                    .map_err(|e| EngineError::Other(format!("saving index: {e}")))?;
            }
        }
        Ok(())
    }

    fn exec_create_index(
        &mut self,
        _txn: TxnId,
        name: String,
        table: String,
        column: String,
    ) -> Result<Output, EngineError> {
        let mut schema = self.schema_of(&table)?;
        let Some(dim) = Self::vector_dim(&schema, &column) else {
            return Err(EngineError::Unsupported(format!(
                "HNSW index requires a VECTOR column; '{column}' is not one"
            )));
        };
        if schema.indexes.iter().any(|ix| {
            ix.name.eq_ignore_ascii_case(&name) || ix.column.eq_ignore_ascii_case(&column)
        }) {
            return Err(EngineError::Other(format!(
                "an index named '{name}' or covering '{column}' already exists"
            )));
        }
        let ix = IndexInfo {
            name,
            column: column.clone(),
            m: 16,
            ef_construction: 200,
        };
        let built = self.build_hnsw(&schema, &ix, dim)?;
        if let Some(p) = self.sidecar_path(&table, &column) {
            built
                .save(&p)
                .map_err(|e| EngineError::Other(format!("saving index: {e}")))?;
        }
        self.hnsw
            .insert(Self::index_map_key(&table, &column), built);
        schema.indexes.push(ix);
        catalog::put_table(&mut self.bp, &mut self.meta, &schema)?;
        Ok(Output::Ack("CREATE INDEX"))
    }

    /// After a row lands in the B+-tree: mirror its vector into each index.
    /// Runs at insert time, not commit time — an abort leaves a ghost the
    /// MVCC fetch filters out (Postgres semantics; see module comment).
    fn hnsw_after_insert(
        &mut self,
        schema: &TableSchema,
        key: &[u8],
        row: &[Value],
    ) -> Result<(), EngineError> {
        if schema.indexes.is_empty() {
            return Ok(());
        }
        for ix in schema.indexes.clone() {
            let Some(dim) = Self::vector_dim(schema, &ix.column) else {
                continue;
            };
            let Some(col) = schema.column_index(&ix.column) else {
                continue;
            };
            if let Value::Vector(v) = &row[col] {
                self.ensure_hnsw(&schema.name, &ix, dim, schema)?;
                let mkey = Self::index_map_key(&schema.name, &ix.column);
                self.hnsw
                    .get_mut(&mkey)
                    .expect("just ensured")
                    .insert(v, key);
            }
        }
        Ok(())
    }

    /// After DELETE stamps tombstones: tombstone the index nodes too.
    fn hnsw_after_delete(&mut self, schema: &TableSchema, keys: &[Vec<u8>]) {
        for ix in &schema.indexes {
            let mkey = Self::index_map_key(&schema.name, &ix.column);
            if let Some(idx) = self.hnsw.get_mut(&mkey) {
                for k in keys {
                    idx.delete_by_key(k);
                }
            }
        }
    }

    // ---- dispatch ---------------------------------------------------------

    fn dispatch(&mut self, txn: TxnId, stmt: Statement) -> Result<Output, EngineError> {
        match stmt {
            Statement::CreateTable { name, columns } => self.exec_create(name, columns),
            Statement::CreateIndex {
                name,
                table,
                column,
            } => self.exec_create_index(txn, name, table, column),
            Statement::DropTable { name } => self.exec_drop(name),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.exec_insert(txn, table, columns, rows),
            Statement::Select(sel) => self.exec_select(txn, sel),
            Statement::Update {
                table,
                assignments,
                filter,
            } => self.exec_update(txn, table, assignments, filter),
            Statement::Delete { table, filter } => self.exec_delete(txn, table, filter),
            Statement::Explain(inner) => match *inner {
                Statement::Select(sel) => self.exec_explain(txn, sel),
                _ => Err(EngineError::Unsupported("EXPLAIN expects a SELECT".into())),
            },
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
            row_count: 0,
            indexes: Vec::new(),
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
            // Update the cardinality statistic before persisting so the write
            // through table_put_chain carries the new count into the catalog.
            schema.row_count += 1;
            self.table_put_chain(&mut schema, &key, &chain)?;
            // Mirror the vector into any HNSW index (M9) — see the vector
            // indexes section for the ghost/abort semantics.
            self.hnsw_after_insert(&schema, &key, &row)?;
            self.write_sets
                .get_mut(&txn)
                .expect("active txn")
                .push((table.clone(), key));
            count += 1;
        }
        Ok(Output::Affected(count))
    }

    // ---- SELECT / EXPLAIN (plan + execute) --------------------------------

    fn exec_select(&mut self, txn: TxnId, sel: Select) -> Result<Output, EngineError> {
        let snap = self.mgr.snapshot(txn).clone();
        let plan = self.build_plan(&sel)?;
        let rs = self.run_plan(&plan, &snap)?;
        Ok(Output::Rows {
            columns: rs.schema.iter().map(|c| c.name.clone()).collect(),
            rows: rs.rows,
        })
    }

    fn exec_explain(&mut self, txn: TxnId, sel: Select) -> Result<Output, EngineError> {
        let _ = txn;
        let plan = self.build_plan(&sel)?;
        let rows = plan
            .explain()
            .into_iter()
            .map(|line| vec![Value::Text(line)])
            .collect();
        Ok(Output::Rows {
            columns: vec!["QUERY PLAN".into()],
            rows,
        })
    }

    /// A base relation participating in a query: its alias, schema and estimated size.
    fn gather_rels(&mut self, sel: &Select) -> Result<Vec<Rel>, EngineError> {
        let mut refs: Vec<&TableRef> = vec![&sel.from];
        refs.extend(sel.joins.iter().map(|j| &j.right));
        let mut rels = Vec::with_capacity(refs.len());
        for r in refs {
            let schema = self.schema_of(&r.name)?;
            let alias = r.key().to_string();
            if rels
                .iter()
                .any(|x: &Rel| x.alias.eq_ignore_ascii_case(&alias))
            {
                return Err(EngineError::Other(format!(
                    "duplicate table name '{alias}'"
                )));
            }
            // Read the cardinality statistic straight from the catalog — no scan.
            let est = schema.row_count as f64;
            rels.push(Rel {
                alias,
                table: r.name.clone(),
                schema,
                est,
            });
        }
        Ok(rels)
    }

    /// Build a physical plan for `sel`.
    ///
    /// The cost-based optimizer engages for all-`INNER` queries of up to 8
    /// relations: it pushes single-relation predicates to scans, picks a PK
    /// index seek where a `pk = const` predicate is available, and orders the
    /// left-deep join tree to minimise estimated intermediate cardinality.
    /// Queries with a `LEFT` join fall back to a written-order left-deep plan
    /// (reordering an outer join is not generally valid).
    fn build_plan(&mut self, sel: &Select) -> Result<Plan, EngineError> {
        let rels = self.gather_rels(sel)?;
        let scope = scope_columns(&rels);

        let all_inner = sel.joins.iter().all(|j| j.join_type == JoinType::Inner);
        let mut node = if all_inner && rels.len() <= 8 {
            build_join_tree_optimized(&rels, sel)
        } else {
            build_join_tree_baseline(&rels, sel)
        };

        // M9: `ORDER BY distance(col, <const>) LIMIT k` over a single table
        // whose `col` has an HNSW index becomes an approximate top-k index
        // scan. Only the *access path* changes: the Sort above re-orders the
        // candidates exactly and Limit truncates, so the index can only
        // affect which candidates are considered, never their ordering —
        // the same contract pgvector's index scans have.
        self.try_vector_access(&mut node, sel, &rels);

        // Aggregation, if any aggregate appears or GROUP BY is present.
        let mut aggs = Vec::new();
        for item in &sel.items {
            if let SelectItem::Expr { expr, .. } = item {
                planner::collect_aggs_in(expr, &mut aggs);
            }
        }
        if let Some(h) = &sel.having {
            planner::collect_aggs_in(h, &mut aggs);
        }
        for ob in &sel.order_by {
            planner::collect_aggs_in(&ob.expr, &mut aggs);
        }
        let aggregate_mode = !aggs.is_empty() || !sel.group_by.is_empty();

        let mut subs: Vec<(Expr, Expr)> = Vec::new();
        let mut project_scope = scope.clone();
        if aggregate_mode {
            let mut out_names = Vec::new();
            for (i, g) in sel.group_by.iter().enumerate() {
                let name = match g {
                    Expr::Column { name, .. } => name.clone(),
                    _ => format!("$grp{i}"),
                };
                subs.push((g.clone(), Expr::col(name.clone())));
                out_names.push(name);
            }
            for (i, a) in aggs.iter().enumerate() {
                let name = format!("$agg{i}");
                subs.push((a.clone(), Expr::col(name.clone())));
                out_names.push(name);
            }
            project_scope = out_names
                .iter()
                .map(|n| Col {
                    table: None,
                    name: n.clone(),
                    ty: DataType::Integer,
                })
                .collect();
            let est = node.est().sqrt().max(1.0);
            node = Plan::Aggregate {
                input: Box::new(node),
                group_by: sel.group_by.clone(),
                aggs,
                out_names,
                est,
            };
        }

        // HAVING (post-aggregation).
        if let Some(h) = &sel.having {
            let pred = planner::substitute(h, &subs);
            let est = node.est() * 0.5;
            node = Plan::Filter {
                input: Box::new(node),
                pred,
                est,
            };
        }

        // Projection (computed before ORDER BY so order keys may reference aliases).
        let (exprs, names) = expand_projection(&sel.items, &project_scope, &subs)?;
        // A bare ORDER BY name that matches an output column resolves to that
        // projected expression (SQL lets ORDER BY reference SELECT aliases).
        let alias_subs: Vec<(Expr, Expr)> = names
            .iter()
            .zip(&exprs)
            .map(|(n, e)| (Expr::col(n.clone()), e.clone()))
            .collect();

        // ORDER BY.
        if !sel.order_by.is_empty() {
            let keys = sel
                .order_by
                .iter()
                .map(|ob| {
                    let e = planner::substitute(&ob.expr, &alias_subs);
                    (planner::substitute(&e, &subs), ob.descending)
                })
                .collect();
            let est = node.est();
            node = Plan::Sort {
                input: Box::new(node),
                keys,
                est,
            };
        }

        // Projection.
        let est = node.est();
        node = Plan::Project {
            input: Box::new(node),
            exprs,
            names,
            est,
        };

        // LIMIT / OFFSET.
        if sel.limit.is_some() || sel.offset.is_some() {
            let est = node.est();
            node = Plan::Limit {
                input: Box::new(node),
                limit: sel.limit,
                offset: sel.offset,
                est,
            };
        }
        Ok(node)
    }

    /// See `build_plan`: swap a Seq scan for an HNSW top-k access when the
    /// query is a k-NN pattern the index can serve.
    fn try_vector_access(&mut self, node: &mut Plan, sel: &Select, rels: &[Rel]) {
        if rels.len() != 1
            || !sel.joins.is_empty()
            || !sel.group_by.is_empty()
            || sel.having.is_some()
            || sel.order_by.len() != 1
            || sel.order_by[0].descending
        {
            return;
        }
        let Some(limit) = sel.limit else { return };
        let Expr::Distance { left, right } = &sel.order_by[0].expr else {
            return;
        };
        let rel = &rels[0];
        // One side must be the indexed column, the other a constant.
        let col_of = |e: &Expr| -> Option<String> {
            if let Expr::Column { table, name } = e {
                let alias_ok = table
                    .as_deref()
                    .is_none_or(|t| t.eq_ignore_ascii_case(&rel.alias));
                if alias_ok && Self::vector_dim(&rel.schema, name).is_some() {
                    return Some(name.clone());
                }
            }
            None
        };
        let (column, query) = match (col_of(left), col_of(right)) {
            (Some(c), None) if is_const(right) => (c, (**right).clone()),
            (None, Some(c)) if is_const(left) => (c, (**left).clone()),
            _ => return,
        };
        if !rel
            .schema
            .indexes
            .iter()
            .any(|ix| ix.column.eq_ignore_ascii_case(&column))
        {
            return;
        }
        let k = (limit + sel.offset.unwrap_or(0)) as usize;
        if let Plan::Scan { access, est, .. } = node {
            if matches!(access, Access::Seq) {
                *access = Access::VectorTopK { column, query, k };
                *est = k as f64;
            }
        }
    }

    fn run_plan(&mut self, plan: &Plan, snap: &Snapshot) -> Result<RowSet, EngineError> {
        match plan {
            Plan::Scan {
                table,
                alias,
                access,
                filter,
                ..
            } => {
                let rs = self.exec_scan(table, alias, access, filter.as_ref(), snap)?;
                match filter {
                    Some(p) => plan::filter_rows(rs, p),
                    None => Ok(rs),
                }
            }
            Plan::Join {
                left,
                right,
                jt,
                on,
                hash_keys,
                ..
            } => {
                let l = self.run_plan(left, snap)?;
                let r = self.run_plan(right, snap)?;
                match hash_keys {
                    Some((lk, rk)) => plan::hash_join(&l, &r, *jt, lk, rk, on),
                    None => plan::nested_loop_join(&l, &r, *jt, on),
                }
            }
            Plan::Filter { input, pred, .. } => {
                let rs = self.run_plan(input, snap)?;
                plan::filter_rows(rs, pred)
            }
            Plan::Aggregate {
                input,
                group_by,
                aggs,
                out_names,
                ..
            } => {
                let rs = self.run_plan(input, snap)?;
                plan::aggregate_rows(rs, group_by, aggs, out_names)
            }
            Plan::Sort { input, keys, .. } => {
                let rs = self.run_plan(input, snap)?;
                plan::sort_rows(rs, keys)
            }
            Plan::Project {
                input,
                exprs,
                names,
                ..
            } => {
                let rs = self.run_plan(input, snap)?;
                plan::project_rows(rs, exprs, names)
            }
            Plan::Limit {
                input,
                limit,
                offset,
                ..
            } => {
                let rs = self.run_plan(input, snap)?;
                Ok(plan::limit_rows(rs, *limit, *offset))
            }
        }
    }

    /// Materialise a base relation's MVCC-visible rows as a `RowSet`.
    /// `residual` is the scan's pushed-down filter: most access paths ignore
    /// it (run_plan applies it after), but the HNSW path threads it into the
    /// graph traversal — predicate-aware search, not post-hoc filtering.
    fn exec_scan(
        &mut self,
        table: &str,
        alias: &str,
        access: &Access,
        residual: Option<&Expr>,
        snap: &Snapshot,
    ) -> Result<RowSet, EngineError> {
        let schema = self.schema_of(table)?;
        let types = schema.types();
        let cols = schema
            .columns
            .iter()
            .map(|c| Col {
                table: Some(alias.to_string()),
                name: c.name.clone(),
                ty: c.data_type,
            })
            .collect();
        let mut rows = Vec::new();
        match access {
            Access::Seq => {
                for (_k, chain) in self.table_scan(schema.root)? {
                    if let Some(i) = mvcc::visible_index(&chain, snap, &self.mgr) {
                        rows.push(tuple::decode_tuple(&types, &chain[i].data)?);
                    }
                }
            }
            Access::IndexSeek { key, .. } => {
                // The planner emits PK equality seeks only.
                let kv = eval::eval(key, &empty_schema(), &[])?;
                let kbytes = tuple::value_to_key(&kv)?;
                if let Some(chain) = self.table_get_chain(schema.root, &kbytes)? {
                    if let Some(i) = mvcc::visible_index(&chain, snap, &self.mgr) {
                        rows.push(tuple::decode_tuple(&types, &chain[i].data)?);
                    }
                }
            }
            Access::VectorTopK { column, query, k } => {
                rows = self.exec_vector_topk(&schema, &types, column, query, *k, residual, snap)?;
            }
            Access::IndexRange { lo, hi } => {
                // Encode the bounds (order-preserving PK keys) and let the
                // B+-tree seek straight to the starting leaf and walk the sibling
                // chain — `lo` inclusive, `hi` exclusive.
                let bound = |e: &Option<Expr>| -> Result<Option<Vec<u8>>, EngineError> {
                    match e {
                        Some(e) => Ok(Some(tuple::value_to_key(&eval::eval(
                            e,
                            &empty_schema(),
                            &[],
                        )?)?)),
                        None => Ok(None),
                    }
                };
                let lo_b = bound(lo)?;
                let hi_b = bound(hi)?;
                let raw = {
                    let mut tree = BPlusTree::open_at(&mut self.bp, schema.root);
                    tree.scan(lo_b.as_deref(), hi_b.as_deref())?
                };
                for (_k, bytes) in raw {
                    let chain = mvcc::decode_chain(&bytes);
                    if let Some(i) = mvcc::visible_index(&chain, snap, &self.mgr) {
                        rows.push(tuple::decode_tuple(&types, &chain[i].data)?);
                    }
                }
            }
        }
        Ok(RowSet { schema: cols, rows })
    }

    /// Execute an HNSW top-k access (M9), the heart of filtered vector search.
    ///
    /// Unfiltered: beam-search the graph, fetch the returned row keys through
    /// the B+-tree, keep the MVCC-visible ones.
    ///
    /// Filtered (`residual` present): the predicate rides *inside* the
    /// traversal (`search_filtered`): non-matching nodes still route the beam
    /// but cannot enter the result set. This is the mitigation for the
    /// post-filter recall cliff — filtering *after* a plain top-k search dies
    /// when the predicate is selective (top-k nearest may contain zero
    /// matches); filtering *candidate admission but not traversal* keeps the
    /// beam connected through non-matching regions. If the beam still comes
    /// back short (ultra-selective predicate), `ef` escalates ×4 until it
    /// covers the graph — at which point the search has degenerated into an
    /// exact filtered scan, which is precisely the right fallback.
    #[allow(clippy::too_many_arguments)]
    fn exec_vector_topk(
        &mut self,
        schema: &TableSchema,
        types: &[DataType],
        column: &str,
        query: &Expr,
        k: usize,
        residual: Option<&Expr>,
        snap: &Snapshot,
    ) -> Result<Vec<Vec<Value>>, EngineError> {
        let ix = schema
            .indexes
            .iter()
            .find(|ix| ix.column.eq_ignore_ascii_case(column))
            .cloned()
            .ok_or_else(|| EngineError::Other("planner chose a missing index".into()))?;
        let dim = Self::vector_dim(schema, column)
            .ok_or_else(|| EngineError::Type(format!("'{column}' is not a vector column")))?;
        self.ensure_hnsw(&schema.name, &ix, dim, schema)?;

        // The query vector: a constant expression ('[...]' text or vector).
        let qv = match eval::eval(query, &empty_schema(), &[])? {
            Value::Vector(v) => v,
            Value::Text(s) => eval::parse_vector_text(&s)?,
            other => {
                return Err(EngineError::Type(format!(
                    "distance() query must be a vector, got {other:?}"
                )))
            }
        };
        if qv.len() != dim as usize {
            return Err(EngineError::Type(format!(
                "query vector has {} dimensions, column '{column}' has {dim}",
                qv.len()
            )));
        }

        // Take the index out of the map so the traversal closure may borrow
        // `self` mutably for B+-tree probes (visibility + predicate checks).
        let mkey = Self::index_map_key(&schema.name, column);
        let idx = self.hnsw.remove(&mkey).expect("just ensured");
        let n = idx.len().max(1);
        let mut ef = (4 * k).max(64);
        let result = loop {
            let found = match residual {
                None => idx.search(&qv, k, ef),
                Some(pred) => {
                    let mut pass = |_id: u32, keyb: &[u8]| -> bool {
                        self.probe_predicate(schema, types, keyb, pred, snap)
                            .unwrap_or(false)
                    };
                    idx.search_filtered(&qv, k, ef, &mut pass)
                }
            };
            // Enough results, or the beam already spans the whole graph
            // (ef ≥ 2n ⇒ the "approximate" search became exhaustive).
            if found.len() >= k || ef >= 2 * n {
                break found;
            }
            ef *= 4;
        };
        self.hnsw.insert(mkey, idx);

        // Resolve row keys through the primary B+-tree, under the snapshot.
        let mut rows = Vec::with_capacity(result.len());
        let root = schema.root;
        for (_d, id) in &result {
            let keyb = {
                // Re-borrow through the map (idx moved back in).
                let idx = self.hnsw.get(&Self::index_map_key(&schema.name, column));
                idx.expect("resident").key(*id).to_vec()
            };
            if let Some(chain) = self.table_get_chain(root, &keyb)? {
                if let Some(i) = mvcc::visible_index(&chain, snap, &self.mgr) {
                    rows.push(tuple::decode_tuple(types, &chain[i].data)?);
                }
            }
        }
        Ok(rows)
    }

    /// Is the row behind `keyb` visible under `snap` AND passing `pred`?
    /// Used as the admission test inside filtered graph traversal.
    fn probe_predicate(
        &mut self,
        schema: &TableSchema,
        types: &[DataType],
        keyb: &[u8],
        pred: &Expr,
        snap: &Snapshot,
    ) -> Result<bool, EngineError> {
        let Some(chain) = self.table_get_chain(schema.root, keyb)? else {
            return Ok(false);
        };
        let Some(i) = mvcc::visible_index(&chain, snap, &self.mgr) else {
            return Ok(false);
        };
        let row = tuple::decode_tuple(types, &chain[i].data)?;
        Ok(matches!(
            eval::eval(pred, schema, &row)?,
            Value::Boolean(true)
        ))
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
        // If an indexed vector column was assigned, the index must follow:
        // tombstone the old node, insert the new vector under the same key.
        let reindex = schema.indexes.iter().any(|ix| {
            assignments
                .iter()
                .any(|(c, _)| c.eq_ignore_ascii_case(&ix.column))
        });
        for (key, chain) in ops {
            self.table_put_chain(&mut schema, &key, &chain)?;
            if reindex {
                self.hnsw_after_delete(&schema, std::slice::from_ref(&key));
                let newrow = tuple::decode_tuple(&types, &chain.last().expect("just pushed").data)?;
                self.hnsw_after_insert(&schema, &key, &newrow)?;
            }
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
        // Decrement the cardinality statistic before persisting the tombstoned
        // chains, so each table_put_chain carries the new count into the catalog.
        schema.row_count = schema.row_count.saturating_sub(count as u64);
        let deleted_keys: Vec<Vec<u8>> = ops.iter().map(|(k, _)| k.clone()).collect();
        for (key, chain) in ops {
            self.table_put_chain(&mut schema, &key, &chain)?;
            self.write_sets
                .get_mut(&txn)
                .expect("active txn")
                .push((table.clone(), key));
        }
        // Tombstone the index nodes (they keep routing, stop being returned).
        self.hnsw_after_delete(&schema, &deleted_keys);
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
        row_count: 0,
        indexes: Vec::new(),
    }
}

/// A base relation participating in a query.
struct Rel {
    alias: String,
    table: String,
    schema: TableSchema,
    est: f64,
}

/// The qualified columns a relation contributes to a query's scope.
fn cols_of(rel: &Rel) -> Vec<Col> {
    rel.schema
        .columns
        .iter()
        .map(|c| Col {
            table: Some(rel.alias.clone()),
            name: c.name.clone(),
            ty: c.data_type,
        })
        .collect()
}

/// The combined scope of all relations, in order.
fn scope_columns(rels: &[Rel]) -> Vec<Col> {
    rels.iter().flat_map(cols_of).collect()
}

/// A sequential-scan plan node for a base relation.
fn scan_plan(rel: &Rel) -> Plan {
    Plan::Scan {
        table: rel.table.clone(),
        alias: rel.alias.clone(),
        access: Access::Seq,
        filter: None,
        est: rel.est,
    }
}

// ---- the cost-based optimizer ---------------------------------------------

/// Split a predicate into its top-level `AND` conjuncts.
fn split_and(e: &Expr, out: &mut Vec<Expr>) {
    if let Expr::Binary {
        op: BinOp::And,
        left,
        right,
    } = e
    {
        split_and(left, out);
        split_and(right, out);
    } else {
        out.push(e.clone());
    }
}

/// `AND` a list of predicates back into one expression (`None` if empty).
fn and_all(exprs: &[Expr]) -> Option<Expr> {
    exprs.iter().cloned().reduce(|a, b| Expr::Binary {
        op: BinOp::And,
        left: Box::new(a),
        right: Box::new(b),
    })
}

/// Collect every column reference in `e`.
fn collect_cols(e: &Expr, out: &mut Vec<(Option<String>, String)>) {
    match e {
        Expr::Column { table, name } => out.push((table.clone(), name.clone())),
        Expr::Binary { left, right, .. } => {
            collect_cols(left, out);
            collect_cols(right, out);
        }
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => collect_cols(expr, out),
        Expr::Distance { left, right } => {
            collect_cols(left, out);
            collect_cols(right, out);
        }
        Expr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                collect_cols(a, out);
            }
        }
        Expr::Literal(_) => {}
    }
}

/// Which relations (as a bitmask) a predicate references.
fn rel_refs(e: &Expr, rels: &[Rel]) -> u32 {
    let mut cols = Vec::new();
    collect_cols(e, &mut cols);
    let mut mask = 0u32;
    for (table, name) in &cols {
        for (i, rel) in rels.iter().enumerate() {
            let has = rel.schema.column_index(name).is_some();
            let alias_ok = table
                .as_deref()
                .is_none_or(|t| t.eq_ignore_ascii_case(&rel.alias));
            if has && alias_ok {
                mask |= 1 << i;
            }
        }
    }
    mask
}

/// A rough selectivity estimate for a predicate (fraction of rows kept).
fn selectivity(e: &Expr) -> f64 {
    match e {
        Expr::Binary { op: BinOp::Eq, .. } => 0.1,
        Expr::Binary { op: BinOp::Ne, .. } => 0.9,
        Expr::Binary {
            op: BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge,
            ..
        } => 0.33,
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
        } => selectivity(left) * selectivity(right),
        Expr::Binary {
            op: BinOp::Or,
            left,
            right,
        } => (selectivity(left) + selectivity(right)).min(1.0),
        _ => 0.5,
    }
}

fn is_const(e: &Expr) -> bool {
    let mut cols = Vec::new();
    collect_cols(e, &mut cols);
    cols.is_empty()
}

/// If `conj` is a sargable `pk = const` on `rel`, return the constant key expression.
fn pk_seek_key(conj: &Expr, rel: &Rel) -> Option<Expr> {
    let pk = rel.schema.pk_index()?;
    let pk_name = &rel.schema.columns[pk].name;
    let is_pk = |e: &Expr| {
        matches!(e, Expr::Column { table, name }
            if name.eq_ignore_ascii_case(pk_name)
            && table.as_deref().is_none_or(|t| t.eq_ignore_ascii_case(&rel.alias)))
    };
    if let Expr::Binary {
        op: BinOp::Eq,
        left,
        right,
    } = conj
    {
        if is_pk(left) && is_const(right) {
            return Some((**right).clone());
        }
        if is_pk(right) && is_const(left) {
            return Some((**left).clone());
        }
    }
    None
}

/// Which side of a range a sargable PK inequality contributes.
enum PkBound {
    /// Inclusive lower bound, usable directly as the B+-tree scan's `lo`.
    Lower(Expr),
    /// Exclusive upper bound, usable directly as the B+-tree scan's `hi`.
    Upper(Expr),
}

/// Flip a comparison operator to normalise `const <op> pk` into `pk <op> const`.
fn flip_op(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

/// If `conj` is a sargable PK inequality on `rel`, return the range bound it
/// implies. Bounds are conservative supersets — the scan's residual filter
/// still enforces exact semantics — so `>` widens to an inclusive lower bound
/// and `<=` yields no index bound at all (it would need the key's successor).
fn pk_range_bound(conj: &Expr, rel: &Rel) -> Option<PkBound> {
    let pk = rel.schema.pk_index()?;
    let pk_name = &rel.schema.columns[pk].name;
    let is_pk = |e: &Expr| {
        matches!(e, Expr::Column { table, name }
            if name.eq_ignore_ascii_case(pk_name)
            && table.as_deref().is_none_or(|t| t.eq_ignore_ascii_case(&rel.alias)))
    };
    let Expr::Binary { op, left, right } = conj else {
        return None;
    };
    let (op, konst) = if is_pk(left) && is_const(right) {
        (*op, (**right).clone())
    } else if is_pk(right) && is_const(left) {
        (flip_op(*op), (**left).clone())
    } else {
        return None;
    };
    match op {
        BinOp::Ge | BinOp::Gt => Some(PkBound::Lower(konst)),
        BinOp::Lt => Some(PkBound::Upper(konst)),
        _ => None,
    }
}

/// Build a base-relation scan, choosing the cheapest available PK access path —
/// an equality index seek, else a B+-tree range scan for PK inequalities, else
/// a sequential scan — and attaching the single-relation predicates as a
/// pushed-down filter. Range predicates stay in the filter for exact semantics.
fn scan_with_pushdown(rel: &Rel, pushed: &[Expr]) -> (Plan, f64) {
    let mut remaining: Vec<Expr> = pushed.to_vec();
    let mut est = rel.est;
    let access = if let Some((idx, key)) = remaining
        .iter()
        .enumerate()
        .find_map(|(j, c)| pk_seek_key(c, rel).map(|k| (j, k)))
    {
        remaining.remove(idx);
        est = 1.0;
        Access::IndexSeek { op: BinOp::Eq, key }
    } else {
        // No equality seek: look for PK range bounds to drive a range scan.
        let mut lo = None;
        let mut hi = None;
        for c in &remaining {
            match pk_range_bound(c, rel) {
                Some(PkBound::Lower(k)) if lo.is_none() => lo = Some(k),
                Some(PkBound::Upper(k)) if hi.is_none() => hi = Some(k),
                _ => {}
            }
        }
        for c in &remaining {
            est *= selectivity(c);
        }
        est = est.max(1.0);
        if lo.is_some() || hi.is_some() {
            Access::IndexRange { lo, hi }
        } else {
            Access::Seq
        }
    };
    let plan = Plan::Scan {
        table: rel.table.clone(),
        alias: rel.alias.clone(),
        access,
        filter: and_all(&remaining),
        est,
    };
    (plan, est)
}

/// Written-order left-deep plan (used when a `LEFT` join blocks reordering).
fn build_join_tree_baseline(rels: &[Rel], sel: &Select) -> Plan {
    let mut node = scan_plan(&rels[0]);
    let mut left_cols = cols_of(&rels[0]);
    for (i, join) in sel.joins.iter().enumerate() {
        let right = &rels[i + 1];
        let right_cols = cols_of(right);
        let hash_keys = planner::detect_hash_keys(&join.on, &left_cols, &right_cols);
        let est = if hash_keys.is_some() {
            node.est().max(right.est)
        } else {
            node.est().max(1.0) * right.est.max(1.0) * 0.3
        };
        node = Plan::Join {
            left: Box::new(node),
            right: Box::new(scan_plan(right)),
            jt: join.join_type,
            on: join.on.clone(),
            hash_keys,
            est,
        };
        left_cols.extend(right_cols);
    }
    if let Some(pred) = &sel.filter {
        let est = node.est() * 0.3;
        node = Plan::Filter {
            input: Box::new(node),
            pred: pred.clone(),
            est,
        };
    }
    node
}

/// System-R-style cost-based left-deep plan for an all-`INNER` query.
///
/// All `ON` and `WHERE` conjuncts are pooled: single-relation conjuncts are
/// pushed to scans, the rest become join predicates. A DP over relation subsets
/// minimises the summed intermediate-result size; each join applies the
/// predicates that first connect its two sides (hash join when one is an equi).
fn build_join_tree_optimized(rels: &[Rel], sel: &Select) -> Plan {
    let n = rels.len();

    // Pool every conjunct and classify by the relations it references.
    let mut conjuncts = Vec::new();
    if let Some(f) = &sel.filter {
        split_and(f, &mut conjuncts);
    }
    for j in &sel.joins {
        split_and(&j.on, &mut conjuncts);
    }
    let mut pushed: Vec<Vec<Expr>> = vec![Vec::new(); n];
    let mut constants: Vec<Expr> = Vec::new();
    let mut join_preds: Vec<(u32, Expr)> = Vec::new();
    for c in conjuncts {
        let refs = rel_refs(&c, rels);
        match refs.count_ones() {
            0 => constants.push(c),
            1 => pushed[refs.trailing_zeros() as usize].push(c),
            _ => join_preds.push((refs, c)),
        }
    }

    // Singleton access paths.
    let mut plans: Vec<Option<(f64, f64, Plan)>> = vec![None; 1 << n]; // (cost, est, plan)
    let mut single_cols: Vec<Vec<Col>> = Vec::with_capacity(n);
    for (i, rel) in rels.iter().enumerate() {
        single_cols.push(cols_of(rel));
        let (plan, est) = scan_with_pushdown(rel, &pushed[i]);
        plans[1 << i] = Some((0.0, est, plan));
    }

    // DP over subsets: right side is always a single relation (left-deep).
    for mask in 1u32..(1 << n) {
        if mask.count_ones() < 2 {
            continue;
        }
        let mut best: Option<(f64, f64, Plan)> = None;
        for r in 0..n {
            let r_bit = 1u32 << r;
            if mask & r_bit == 0 {
                continue;
            }
            let left_mask = mask & !r_bit;
            let Some((left_cost, left_est, left_plan)) = &plans[left_mask as usize] else {
                continue;
            };
            let Some((_, right_est, right_plan)) = &plans[r_bit as usize] else {
                continue;
            };

            // Predicates that first connect `left_mask` and `r`.
            let applicable: Vec<Expr> = join_preds
                .iter()
                .filter(|(refs, _)| refs & !mask == 0 && refs & left_mask != 0 && refs & r_bit != 0)
                .map(|(_, e)| e.clone())
                .collect();

            let sel_factor: f64 = applicable.iter().map(selectivity).product();
            let est = (left_est * right_est * sel_factor).max(1.0);
            let cost = left_cost + est;

            if best.as_ref().is_none_or(|(bc, _, _)| cost < *bc) {
                let left_cols: Vec<Col> = (0..n)
                    .filter(|i| left_mask & (1 << i) != 0)
                    .flat_map(|i| single_cols[i].clone())
                    .collect();
                let hash_keys = applicable
                    .iter()
                    .find_map(|p| planner::detect_hash_keys(p, &left_cols, &single_cols[r]));
                let on = and_all(&applicable).unwrap_or(Expr::Literal(Value::Boolean(true)));
                let plan = Plan::Join {
                    left: Box::new(left_plan.clone()),
                    right: Box::new(right_plan.clone()),
                    jt: JoinType::Inner,
                    on,
                    hash_keys,
                    est,
                };
                best = Some((cost, est, plan));
            }
        }
        plans[mask as usize] = best;
    }

    let (_, _, mut node) = plans[(1 << n) - 1]
        .clone()
        .expect("a full-set plan always exists");

    // Constant predicates (no relation references) apply as a top filter.
    if let Some(pred) = and_all(&constants) {
        let est = node.est();
        node = Plan::Filter {
            input: Box::new(node),
            pred,
            est,
        };
    }
    node
}

/// Expand `SELECT` items into projection expressions and output column names,
/// applying `subs` (the aggregate/group substitutions) to each expression.
fn expand_projection(
    items: &[SelectItem],
    scope: &[Col],
    subs: &[(Expr, Expr)],
) -> Result<(Vec<Expr>, Vec<String>), EngineError> {
    let mut exprs = Vec::new();
    let mut names = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for c in scope {
                    exprs.push(Expr::Column {
                        table: c.table.clone(),
                        name: c.name.clone(),
                    });
                    names.push(c.name.clone());
                }
            }
            SelectItem::QualifiedWildcard(alias) => {
                let mut any = false;
                for c in scope {
                    if c.table
                        .as_deref()
                        .is_some_and(|t| t.eq_ignore_ascii_case(alias))
                    {
                        exprs.push(Expr::Column {
                            table: c.table.clone(),
                            name: c.name.clone(),
                        });
                        names.push(c.name.clone());
                        any = true;
                    }
                }
                if !any {
                    return Err(EngineError::UnknownTable(alias.clone()));
                }
            }
            SelectItem::Expr { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| planner::default_name(expr));
                let projected = planner::substitute(expr, subs);
                planner::validate_cols(&projected, scope)?;
                exprs.push(projected);
                names.push(name);
            }
        }
    }
    Ok((exprs, names))
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
        DataType::Vector(dim) => {
            let vec = match v {
                Value::Null => return null_ok(col),
                Value::Vector(x) => x,
                // pgvector-style: a quoted text literal '[0.1, 0.2, ...]'.
                Value::Text(s) => eval::parse_vector_text(&s)?,
                other => {
                    return Err(EngineError::Type(format!(
                        "column '{}' expects VECTOR({dim}), got {other:?}",
                        col.name
                    )))
                }
            };
            // The dimension is part of the type: enforce it at the door.
            if vec.len() != dim as usize {
                return Err(EngineError::Type(format!(
                    "column '{}' expects VECTOR({dim}), got a {}-dimensional value",
                    col.name,
                    vec.len()
                )));
            }
            Ok(Value::Vector(vec))
        }
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
