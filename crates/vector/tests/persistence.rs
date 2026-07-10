//! Persistence round-trip: build → save → load → *identical* search results,
//! plus corruption and truncation detection. Mirrors the storage crate's
//! "prove durability, don't assert it" test style.

use vector::distance::Metric;
use vector::hnsw::{Hnsw, HnswParams};

fn build_sample(n: usize, dim: usize) -> Hnsw {
    let mut h = Hnsw::new(dim, Metric::L2, HnswParams::default(), 7);
    // Deterministic pseudo-random vectors (xorshift), keys = row ids.
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for i in 0..n {
        let v: Vec<f32> = (0..dim)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                ((s >> 40) as f32) / (1u64 << 24) as f32
            })
            .collect();
        h.insert(&v, &(i as u64).to_be_bytes());
    }
    h
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(name);
    std::mem::forget(dir); // keep it alive for the test process
    p
}

fn queries(dim: usize, nq: usize) -> Vec<Vec<f32>> {
    let mut s = 0xDEAD_BEEF_CAFE_F00Du64;
    (0..nq)
        .map(|_| {
            (0..dim)
                .map(|_| {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    ((s >> 40) as f32) / (1u64 << 24) as f32
                })
                .collect()
        })
        .collect()
}

#[test]
fn roundtrip_preserves_search_results_exactly() {
    let h = build_sample(800, 16);
    let path = tmp("idx.hnsw");
    h.save(&path).unwrap();

    let mmapped = Hnsw::load(&path).unwrap();
    let owned = Hnsw::load_owned(&path).unwrap();

    assert_eq!(mmapped.len(), h.len());
    assert_eq!(mmapped.dim(), h.dim());
    for q in queries(16, 25) {
        let want = h.search(&q, 10, 64);
        let got_m = mmapped.search(&q, 10, 64);
        let got_o = owned.search(&q, 10, 64);
        // Same graph + same vectors + same deterministic traversal
        // ⇒ bit-identical results, not merely close ones.
        assert_eq!(want, got_m, "mmap-backed load must not change results");
        assert_eq!(want, got_o, "owned load must not change results");
    }
    // Row keys survive.
    assert_eq!(mmapped.key(42), 42u64.to_be_bytes());
}

#[test]
fn reloaded_index_accepts_new_inserts() {
    let h = build_sample(200, 8);
    let path = tmp("grow.hnsw");
    h.save(&path).unwrap();

    // Inserting into an mmap-backed index upgrades the arena to owned
    // (copy-on-write) and continues the same deterministic RNG stream.
    let mut re = Hnsw::load(&path).unwrap();
    let v = vec![0.25f32; 8];
    let id = re.insert(&v, b"fresh");
    assert_eq!(id as usize, 200);
    let r = re.search(&v, 1, 32);
    assert_eq!(r[0].1, id);

    // And the grown index round-trips again.
    let path2 = tmp("grow2.hnsw");
    re.save(&path2).unwrap();
    let re2 = Hnsw::load(&path2).unwrap();
    assert_eq!(re2.len(), 201);
    assert_eq!(re2.key(id), b"fresh");
}

#[test]
fn tombstones_survive_a_roundtrip() {
    let mut h = build_sample(100, 8);
    assert!(h.delete_by_key(&7u64.to_be_bytes()));
    let path = tmp("tomb.hnsw");
    h.save(&path).unwrap();
    let re = Hnsw::load(&path).unwrap();
    assert_eq!(re.tombstones(), 1);
    for q in queries(8, 10) {
        assert!(re.search(&q, 100, 200).iter().all(|&(_, id)| id != 7));
    }
}

#[test]
fn corrupted_graph_is_rejected() {
    let h = build_sample(150, 8);
    let path = tmp("corrupt.hnsw");
    h.save(&path).unwrap();
    // Flip one byte inside the graph section (past the 68-byte header).
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[68 + 10] ^= 0xFF;
    std::fs::write(&path, &bytes).unwrap();
    let err = match Hnsw::load(&path) {
        Err(e) => e,
        Ok(_) => panic!("corrupted file loaded successfully"),
    };
    assert!(err.to_string().contains("checksum"), "got: {err}");
}

#[test]
fn truncated_file_is_rejected() {
    let h = build_sample(150, 8);
    let path = tmp("torn.hnsw");
    h.save(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    // Cut the file mid-arena (simulates a crash mid-write of a non-atomic
    // copy; the real writer is atomic via tmp+rename, so this models an
    // external truncation).
    std::fs::write(&path, &bytes[..bytes.len() - 64]).unwrap();
    assert!(Hnsw::load(&path).is_err());
}

#[test]
fn wrong_magic_is_rejected() {
    let path = tmp("notanindex.hnsw");
    std::fs::write(
        &path,
        [b"definitely not an hnsw file, sorry! ".as_slice(); 4].concat(),
    )
    .unwrap();
    let err = match Hnsw::load(&path) {
        Err(e) => e,
        Ok(_) => panic!("garbage file loaded successfully"),
    };
    assert!(err.to_string().contains("magic"), "got: {err}");
}

#[test]
fn empty_index_roundtrips() {
    let h = Hnsw::new(4, Metric::Cosine, HnswParams::default(), 1);
    let path = tmp("empty.hnsw");
    h.save(&path).unwrap();
    let re = Hnsw::load(&path).unwrap();
    assert_eq!(re.len(), 0);
    assert_eq!(re.metric(), Metric::Cosine);
    assert!(re.search(&[0.0; 4], 5, 10).is_empty());
}
