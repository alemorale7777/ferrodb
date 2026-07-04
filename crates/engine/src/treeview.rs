//! A read-only view of a table's B+-tree, for the web playground's visualizer.
//!
//! Walks the physical nodes (via the buffer pool) into a small tree of decoded
//! keys and emits JSON by hand — no serialization dependency.

use sql::ast::DataType;
use storage::btree::node::{self, NodeKind};
use storage::encoding::decode_i64;

/// How a table's B+-tree keys were encoded, so they can be rendered for display.
#[derive(Clone, Copy)]
pub enum KeyKind {
    /// Order-preserving `i64` (INTEGER primary key).
    Int,
    /// Big-endian `u64` hidden row id (no primary key).
    Rowid,
    /// Raw UTF-8 (TEXT primary key).
    Text,
    /// Anything else — rendered as hex.
    Raw,
}

impl KeyKind {
    /// The key kind for a table given its primary-key column type (if any).
    pub fn for_pk(pk_type: Option<DataType>) -> KeyKind {
        match pk_type {
            Some(DataType::Integer) => KeyKind::Int,
            Some(DataType::Text) => KeyKind::Text,
            Some(_) => KeyKind::Raw,
            None => KeyKind::Rowid,
        }
    }

    /// Render an encoded key to a display string.
    pub fn render(self, bytes: &[u8]) -> String {
        match self {
            KeyKind::Int if bytes.len() == 8 => decode_i64(bytes).to_string(),
            KeyKind::Rowid if bytes.len() == 8 => {
                u64::from_be_bytes(bytes.try_into().unwrap()).to_string()
            }
            KeyKind::Text => String::from_utf8_lossy(bytes).into_owned(),
            _ => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

/// One node in the visualized tree.
pub struct TreeNode {
    pub leaf: bool,
    pub keys: Vec<String>,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// Build a leaf node from a physical leaf page's entries.
    pub fn leaf(page: &storage::page::Page, kind: KeyKind) -> TreeNode {
        let keys = node::leaf_entries(page)
            .iter()
            .map(|(k, _)| kind.render(k))
            .collect();
        TreeNode {
            leaf: true,
            keys,
            children: Vec::new(),
        }
    }

    /// The number of keys stored in the leaves of this subtree.
    pub fn leaf_key_count(&self) -> usize {
        if self.leaf {
            self.keys.len()
        } else {
            self.children.iter().map(TreeNode::leaf_key_count).sum()
        }
    }

    /// The height of this subtree (a single leaf has height 1).
    pub fn height(&self) -> usize {
        if self.leaf {
            1
        } else {
            1 + self
                .children
                .iter()
                .map(TreeNode::height)
                .max()
                .unwrap_or(0)
        }
    }

    /// Serialize to JSON: `{"leaf":bool,"keys":[...],"children":[...]}`.
    pub fn to_json(&self) -> String {
        let keys: Vec<String> = self.keys.iter().map(|k| json_string(k)).collect();
        let children: Vec<String> = self.children.iter().map(TreeNode::to_json).collect();
        format!(
            "{{\"leaf\":{},\"keys\":[{}],\"children\":[{}]}}",
            self.leaf,
            keys.join(","),
            children.join(",")
        )
    }
}

/// Is `page` a leaf node?
pub fn is_leaf(page: &storage::page::Page) -> bool {
    node::read_kind(page) == NodeKind::Leaf
}

/// Escape a string as a JSON string literal.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
