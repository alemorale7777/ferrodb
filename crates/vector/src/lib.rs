//! ferrodb vector index (Milestone 9).
//!
//! An HNSW (Hierarchical Navigable Small World) approximate-nearest-neighbor
//! index, built to sit beside the B+-tree as a **secondary index**: searches
//! return row keys, and the engine resolves them through the primary B+-tree —
//! the same division of labor pgvector has with Postgres's heap.
//!
//! Layering (bottom-up, like the storage crate):
//!   [`distance`] — the hot-loop kernels (scalar reference + runtime-dispatched
//!   AVX2), then the HNSW graph, persistence, and the recall harness on top.
//!
//! No third-party crate does the work here — that is the point.

pub mod distance;
pub mod hnsw;
pub mod node;
pub mod persistence;
