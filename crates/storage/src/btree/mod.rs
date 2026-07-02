//! A B+-tree over the pager: ordered map with search, insert (with node
//! splits), range scan, and delete.

pub mod node;
pub mod overflow;
pub mod tree;
