use std::collections::BTreeMap;

use proptest::prelude::*;
use storage::btree::tree::{load_meta, BPlusTree};
use storage::buffer::BufferPool;
use storage::disk::DiskManager;
use storage::encoding::{decode_i64, encode_i64};

proptest! {
    #[test]
    fn btree_matches_btreemap(ops in prop::collection::vec((any::<i8>(), 0i64..64, 0u8..16), 0..400)) {
        let dir = tempfile::tempdir().unwrap();
        let dm = DiskManager::open(dir.path().join("p.db")).unwrap();
        let mut bp = BufferPool::new(dm, 32);
        let mut meta = load_meta(&mut bp).unwrap();
        let mut t = BPlusTree::open(&mut bp, &mut meta);
        let mut model: BTreeMap<i64, Vec<u8>> = BTreeMap::new();

        for (op, k, v) in ops {
            let key = encode_i64(k);
            if op % 3 == 0 {
                let had = t.delete(&key).unwrap();
                prop_assert_eq!(had, model.remove(&k).is_some());
            } else {
                let val = vec![v];
                t.insert(&key, &val).unwrap();
                model.insert(k, val);
            }
        }

        let got: Vec<(i64, Vec<u8>)> = t.scan(None, None).unwrap()
            .into_iter().map(|(k, v)| (decode_i64(&k), v)).collect();
        let want: Vec<(i64, Vec<u8>)> = model.into_iter().collect();
        prop_assert_eq!(got, want);
    }
}
