use storage::btree::node;
use storage::page::{Page, PageId};

#[test]
fn leaf_header_and_cell_roundtrip() {
    let mut page = Page::new_zeroed();
    node::init_leaf(&mut page, Some(PageId(9)));
    node::leaf_put(&mut page, b"b", b"2").unwrap();
    node::leaf_put(&mut page, b"a", b"1").unwrap();
    assert_eq!(node::read_next_leaf(&page), Some(PageId(9)));
    assert_eq!(node::num_entries(&page), 2);

    let (slot, found) = node::search_key(&page, b"a");
    assert!(found);
    let (k, v) = node::leaf_at(&page, slot);
    assert_eq!((k.as_slice(), v.as_slice()), (b"a".as_ref(), b"1".as_ref()));

    let (_slot, found_missing) = node::search_key(&page, b"z");
    assert!(!found_missing);
}

#[test]
fn internal_children_and_left() {
    let mut page = Page::new_zeroed();
    node::init_internal(&mut page, PageId(1));
    node::internal_put(&mut page, b"m", PageId(2)).unwrap();
    node::internal_put(&mut page, b"t", PageId(3)).unwrap();
    assert_eq!(node::left_child(&page), PageId(1));
    assert_eq!(node::internal_at(&page, 1), (b"m".to_vec(), PageId(2)));
    assert_eq!(node::internal_at(&page, 2), (b"t".to_vec(), PageId(3)));
}
