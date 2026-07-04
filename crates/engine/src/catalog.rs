//! The catalog: table schemas stored in a B+-tree keyed by table name.
//!
//! The catalog tree's root lives in the storage `MetaPage.tree_root`; each table
//! schema records its own data-tree root. All persisted in the same file via M1.

use storage::btree::tree::BPlusTree;
use storage::buffer::BufferPool;
use storage::meta::MetaPage;
use storage::page::PageId;

use sql::ast::DataType;

use crate::EngineError;

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: DataType,
    pub not_null: bool,
    pub primary_key: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    pub root: PageId,
    pub next_rowid: u64,
    /// Approximate count of live rows, maintained incrementally on
    /// INSERT/DELETE. The optimizer reads this as a cardinality statistic so
    /// planning never has to scan the table (cf. PostgreSQL's `reltuples`).
    /// It is a heuristic — snapshot-independent and not exact under concurrency.
    pub row_count: u64,
}

impl TableSchema {
    pub fn pk_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }
    pub fn types(&self) -> Vec<DataType> {
        self.columns.iter().map(|c| c.data_type).collect()
    }
}

fn type_tag(t: DataType) -> u8 {
    match t {
        DataType::Integer => 0,
        DataType::Real => 1,
        DataType::Text => 2,
        DataType::Boolean => 3,
    }
}
fn tag_type(b: u8) -> Result<DataType, EngineError> {
    Ok(match b {
        0 => DataType::Integer,
        1 => DataType::Real,
        2 => DataType::Text,
        3 => DataType::Boolean,
        _ => return Err(EngineError::Type("corrupt catalog type tag".into())),
    })
}

fn encode_schema(s: &TableSchema) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(s.columns.len() as u16).to_le_bytes());
    for c in &s.columns {
        out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        out.extend_from_slice(c.name.as_bytes());
        out.push(type_tag(c.data_type));
        out.push((c.not_null as u8) | ((c.primary_key as u8) << 1));
    }
    out.extend_from_slice(&s.root.0.to_le_bytes());
    out.extend_from_slice(&s.next_rowid.to_le_bytes());
    out.extend_from_slice(&s.row_count.to_le_bytes());
    out
}

fn decode_schema(name: &str, bytes: &[u8]) -> Result<TableSchema, EngineError> {
    let corrupt = || EngineError::Type("corrupt catalog row".into());
    let ncols =
        u16::from_le_bytes(bytes.get(0..2).ok_or_else(corrupt)?.try_into().unwrap()) as usize;
    let mut pos = 2;
    let mut columns = Vec::with_capacity(ncols);
    for _ in 0..ncols {
        let nlen = u16::from_le_bytes(
            bytes
                .get(pos..pos + 2)
                .ok_or_else(corrupt)?
                .try_into()
                .unwrap(),
        ) as usize;
        pos += 2;
        let name_b = bytes.get(pos..pos + nlen).ok_or_else(corrupt)?;
        let col_name = String::from_utf8(name_b.to_vec()).map_err(|_| corrupt())?;
        pos += nlen;
        let ty = tag_type(*bytes.get(pos).ok_or_else(corrupt)?)?;
        pos += 1;
        let flags = *bytes.get(pos).ok_or_else(corrupt)?;
        pos += 1;
        columns.push(ColumnInfo {
            name: col_name,
            data_type: ty,
            not_null: flags & 1 == 1,
            primary_key: (flags >> 1) & 1 == 1,
        });
    }
    let root = PageId(u32::from_le_bytes(
        bytes
            .get(pos..pos + 4)
            .ok_or_else(corrupt)?
            .try_into()
            .unwrap(),
    ));
    pos += 4;
    let next_rowid = u64::from_le_bytes(
        bytes
            .get(pos..pos + 8)
            .ok_or_else(corrupt)?
            .try_into()
            .unwrap(),
    );
    pos += 8;
    // row_count was added later; tolerate catalogs written before it existed.
    let row_count = match bytes.get(pos..pos + 8) {
        Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
        None => 0,
    };
    Ok(TableSchema {
        name: name.to_string(),
        columns,
        root,
        next_rowid,
        row_count,
    })
}

/// Ensure the catalog tree exists, returning its root page id.
fn catalog_root(bp: &mut BufferPool, meta: &mut MetaPage) -> Result<PageId, EngineError> {
    if let Some(r) = meta.tree_root {
        return Ok(r);
    }
    let tree = BPlusTree::create(bp)?;
    let root = tree.root();
    meta.tree_root = Some(root);
    Ok(root)
}

pub fn get_table(
    bp: &mut BufferPool,
    meta: &MetaPage,
    name: &str,
) -> Result<Option<TableSchema>, EngineError> {
    let Some(root) = meta.tree_root else {
        return Ok(None);
    };
    let mut tree = BPlusTree::open_at(bp, root);
    match tree.get(name.as_bytes())? {
        Some(bytes) => Ok(Some(decode_schema(name, &bytes)?)),
        None => Ok(None),
    }
}

/// Insert or overwrite a table's schema; updates `meta.tree_root` if it changed.
pub fn put_table(
    bp: &mut BufferPool,
    meta: &mut MetaPage,
    schema: &TableSchema,
) -> Result<(), EngineError> {
    let root = catalog_root(bp, meta)?;
    let mut tree = BPlusTree::open_at(bp, root);
    tree.insert(schema.name.as_bytes(), &encode_schema(schema))?;
    meta.tree_root = Some(tree.root());
    Ok(())
}

pub fn drop_table(
    bp: &mut BufferPool,
    meta: &mut MetaPage,
    name: &str,
) -> Result<bool, EngineError> {
    let Some(root) = meta.tree_root else {
        return Ok(false);
    };
    let mut tree = BPlusTree::open_at(bp, root);
    let existed = tree.delete(name.as_bytes())?;
    meta.tree_root = Some(tree.root());
    Ok(existed)
}

pub fn list_tables(bp: &mut BufferPool, meta: &MetaPage) -> Result<Vec<String>, EngineError> {
    let Some(root) = meta.tree_root else {
        return Ok(Vec::new());
    };
    let mut tree = BPlusTree::open_at(bp, root);
    let rows = tree.scan(None, None)?;
    Ok(rows
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k).into_owned())
        .collect())
}
