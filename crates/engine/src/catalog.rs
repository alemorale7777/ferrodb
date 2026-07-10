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

/// A registered secondary index (M9: HNSW over one vector column).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexInfo {
    pub name: String,
    pub column: String,
    pub m: u32,
    pub ef_construction: u32,
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
    /// Secondary indexes (M9). Encoded after `row_count`; catalogs written
    /// before M9 decode with an empty list — the same tolerate-missing-tail
    /// pattern `row_count` itself used.
    pub indexes: Vec<IndexInfo>,
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

/// Encode a column type: one tag byte, plus a u16 dimension for vectors
/// (the only variable-width type — the dimension is part of the type).
fn encode_type(t: DataType, out: &mut Vec<u8>) {
    match t {
        DataType::Integer => out.push(0),
        DataType::Real => out.push(1),
        DataType::Text => out.push(2),
        DataType::Boolean => out.push(3),
        DataType::Vector(dim) => {
            out.push(4);
            out.extend_from_slice(&dim.to_le_bytes());
        }
    }
}
fn decode_type(bytes: &[u8], pos: &mut usize) -> Result<DataType, EngineError> {
    let corrupt = || EngineError::Type("corrupt catalog type tag".into());
    let tag = *bytes.get(*pos).ok_or_else(corrupt)?;
    *pos += 1;
    Ok(match tag {
        0 => DataType::Integer,
        1 => DataType::Real,
        2 => DataType::Text,
        3 => DataType::Boolean,
        4 => {
            let d = bytes.get(*pos..*pos + 2).ok_or_else(corrupt)?;
            *pos += 2;
            DataType::Vector(u16::from_le_bytes(d.try_into().unwrap()))
        }
        _ => return Err(corrupt()),
    })
}

fn encode_schema(s: &TableSchema) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(s.columns.len() as u16).to_le_bytes());
    for c in &s.columns {
        out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        out.extend_from_slice(c.name.as_bytes());
        encode_type(c.data_type, &mut out);
        out.push((c.not_null as u8) | ((c.primary_key as u8) << 1));
    }
    out.extend_from_slice(&s.root.0.to_le_bytes());
    out.extend_from_slice(&s.next_rowid.to_le_bytes());
    out.extend_from_slice(&s.row_count.to_le_bytes());
    // M9: secondary indexes, after everything older decoders read.
    out.extend_from_slice(&(s.indexes.len() as u16).to_le_bytes());
    for ix in &s.indexes {
        for text in [&ix.name, &ix.column] {
            out.extend_from_slice(&(text.len() as u16).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        out.extend_from_slice(&ix.m.to_le_bytes());
        out.extend_from_slice(&ix.ef_construction.to_le_bytes());
    }
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
        let ty = decode_type(bytes, &mut pos)?;
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
        Some(b) => {
            pos += 8;
            u64::from_le_bytes(b.try_into().unwrap())
        }
        None => 0,
    };
    // Indexes (M9) were added later still; same tolerance.
    let mut indexes = Vec::new();
    if let Some(b) = bytes.get(pos..pos + 2) {
        let n = u16::from_le_bytes(b.try_into().unwrap()) as usize;
        pos += 2;
        for _ in 0..n {
            let text = |pos: &mut usize| -> Result<String, EngineError> {
                let lb = bytes.get(*pos..*pos + 2).ok_or_else(corrupt)?;
                let len = u16::from_le_bytes(lb.try_into().unwrap()) as usize;
                *pos += 2;
                let sb = bytes.get(*pos..*pos + len).ok_or_else(corrupt)?;
                *pos += len;
                String::from_utf8(sb.to_vec()).map_err(|_| corrupt())
            };
            let iname = text(&mut pos)?;
            let column = text(&mut pos)?;
            let mb = bytes.get(pos..pos + 8).ok_or_else(corrupt)?;
            let m = u32::from_le_bytes(mb[0..4].try_into().unwrap());
            let ef_construction = u32::from_le_bytes(mb[4..8].try_into().unwrap());
            pos += 8;
            indexes.push(IndexInfo {
                name: iname,
                column,
                m,
                ef_construction,
            });
        }
    }
    Ok(TableSchema {
        name: name.to_string(),
        columns,
        root,
        next_rowid,
        row_count,
        indexes,
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
