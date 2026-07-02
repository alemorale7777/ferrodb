use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::encoding::{decode_i64, encode_i64};
use storage::meta::MetaPage;

fn fresh() -> (BufferPool, MetaPage) {
    let dir = tempfile::tempdir().unwrap();
    let dm = DiskManager::open(dir.path().join("t.db")).unwrap();
    std::mem::forget(dir); // keep the temp file alive for the test process
    let mut bp = BufferPool::new(dm, 64);
    let meta = load_meta(&mut bp).unwrap();
    (bp, meta)
}

#[test]
fn insert_and_get_many_forces_splits() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    for i in 0..2000i64 {
        t.insert(&encode_i64(i), format!("v{i}").as_bytes())
            .unwrap();
    }
    for i in 0..2000i64 {
        assert_eq!(
            t.get(&encode_i64(i)).unwrap(),
            Some(format!("v{i}").into_bytes()),
            "key {i}"
        );
    }
    assert_eq!(t.get(&encode_i64(9999)).unwrap(), None);
}

#[test]
fn update_overwrites_value() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    t.insert(&encode_i64(1), b"first").unwrap();
    t.insert(&encode_i64(1), b"second").unwrap();
    assert_eq!(t.get(&encode_i64(1)).unwrap(), Some(b"second".to_vec()));
}

#[test]
fn range_scan_is_sorted_and_bounded() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    for i in 0..500i64 {
        t.insert(&encode_i64(i), b"x").unwrap();
    }
    let rows = t
        .scan(Some(&encode_i64(10)), Some(&encode_i64(20)))
        .unwrap();
    let keys: Vec<i64> = rows.iter().map(|(k, _)| decode_i64(k)).collect();
    assert_eq!(keys, (10..20).collect::<Vec<_>>());
}

#[test]
fn full_scan_returns_everything_sorted() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    for i in [5i64, 1, 9, 3, 7, 0, 2, 8, 4, 6] {
        t.insert(&encode_i64(i), b"x").unwrap();
    }
    let keys: Vec<i64> = t
        .scan(None, None)
        .unwrap()
        .iter()
        .map(|(k, _)| decode_i64(k))
        .collect();
    assert_eq!(keys, (0..10).collect::<Vec<_>>());
}

#[test]
fn large_values_roundtrip_via_overflow() {
    let (mut bp, mut meta) = fresh();
    let mut t = BPlusTree::open(&mut bp, &mut meta);
    let big = vec![0xABu8; 20_000]; // spans ~5 overflow pages
    t.insert(&encode_i64(1), &big).unwrap();
    t.insert(&encode_i64(2), b"small").unwrap();
    assert_eq!(t.get(&encode_i64(1)).unwrap(), Some(big));
    assert_eq!(t.get(&encode_i64(2)).unwrap(), Some(b"small".to_vec()));
}

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.db");
    {
        let dm = DiskManager::open(&path).unwrap();
        let mut bp = BufferPool::new(dm, 16);
        let mut meta = load_meta(&mut bp).unwrap();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        for i in 0..300i64 {
            t.insert(&encode_i64(i), b"v").unwrap();
        }
        t.checkpoint().unwrap();
    }
    {
        let dm = DiskManager::open(&path).unwrap();
        let mut bp = BufferPool::new(dm, 16);
        let mut meta = load_meta(&mut bp).unwrap();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        assert_eq!(t.get(&encode_i64(0)).unwrap(), Some(b"v".to_vec()));
        assert_eq!(t.get(&encode_i64(299)).unwrap(), Some(b"v".to_vec()));
    }
}
