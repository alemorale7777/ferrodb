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

use catalog::{ColumnInfo, TableSchema};
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

    // ---- SELECT / EXPLAIN (plan + execute) --------------------------------

    fn exec_select(&mut self, txn: TxnId, sel: Select) -> Result<Output, EngineError> {
        let snap = self.mgr.snapshot(txn).clone();
        let plan = self.build_plan(&sel, &snap)?;
        let rs = self.run_plan(&plan, &snap)?;
        Ok(Output::Rows {
            columns: rs.schema.iter().map(|c| c.name.clone()).collect(),
            rows: rs.rows,
        })
    }

    fn exec_explain(&mut self, txn: TxnId, sel: Select) -> Result<Output, EngineError> {
        let snap = self.mgr.snapshot(txn).clone();
        let plan = self.build_plan(&sel, &snap)?;
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
    fn gather_rels(&mut self, sel: &Select, snap: &Snapshot) -> Result<Vec<Rel>, EngineError> {
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
            let est = self.count_visible(&schema, snap)? as f64;
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
    fn build_plan(&mut self, sel: &Select, snap: &Snapshot) -> Result<Plan, EngineError> {
        let rels = self.gather_rels(sel, snap)?;
        let scope = scope_columns(&rels);

        let all_inner = sel.joins.iter().all(|j| j.join_type == JoinType::Inner);
        let mut node = if all_inner && rels.len() <= 8 {
            build_join_tree_optimized(&rels, sel)
        } else {
            build_join_tree_baseline(&rels, sel)
        };

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

    fn run_plan(&mut self, plan: &Plan, snap: &Snapshot) -> Result<RowSet, EngineError> {
        match plan {
            Plan::Scan {
                table,
                alias,
                access,
                filter,
                ..
            } => {
                let rs = self.exec_scan(table, alias, access, snap)?;
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
    fn exec_scan(
        &mut self,
        table: &str,
        alias: &str,
        access: &Access,
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
        }
        Ok(RowSet { schema: cols, rows })
    }

    fn count_visible(
        &mut self,
        schema: &TableSchema,
        snap: &Snapshot,
    ) -> Result<usize, EngineError> {
        let mut n = 0;
        for (_k, chain) in self.table_scan(schema.root)? {
            if mvcc::visible_index(&chain, snap, &self.mgr).is_some() {
                n += 1;
            }
        }
        Ok(n)
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

/// Build a base-relation scan, choosing a PK index seek when possible and
/// attaching any remaining single-relation predicates as a pushed-down filter.
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
        for c in &remaining {
            est *= selectivity(c);
        }
        est = est.max(1.0);
        Access::Seq
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
