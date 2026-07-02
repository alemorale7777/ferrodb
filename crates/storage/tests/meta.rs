use storage::meta::MetaPage;
use storage::page::PageId;

#[test]
fn meta_encode_decode_roundtrip() {
    let m = MetaPage {
        magic: MetaPage::MAGIC,
        version: 1,
        free_list_head: None,
        tree_root: Some(PageId(5)),
    };
    let page = m.encode();
    let back = MetaPage::decode(&page).unwrap();
    assert_eq!(back.tree_root, Some(PageId(5)));
    assert_eq!(back.free_list_head, None);
    assert_eq!(back.magic, MetaPage::MAGIC);
}

#[test]
fn meta_rejects_bad_magic() {
    let m = MetaPage {
        magic: 0,
        version: 1,
        free_list_head: None,
        tree_root: None,
    };
    let page = m.encode();
    assert!(MetaPage::decode(&page).is_err());
}
