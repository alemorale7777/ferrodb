use storage::page::Page;
use storage::slotted::SlottedPage;

#[test]
fn slotted_insert_get_remove_keeps_order() {
    let mut page = Page::new_zeroed();
    let mut sp = SlottedPage::new(&mut page);
    sp.init();
    sp.insert(0, b"alpha").unwrap();
    sp.insert(1, b"gamma").unwrap();
    sp.insert(1, b"beta").unwrap(); // shift gamma to slot 2
    assert_eq!(sp.num_slots(), 3);
    assert_eq!(sp.get(0), b"alpha");
    assert_eq!(sp.get(1), b"beta");
    assert_eq!(sp.get(2), b"gamma");
    sp.remove(1);
    assert_eq!(sp.num_slots(), 2);
    assert_eq!(sp.get(1), b"gamma");
}

#[test]
fn slotted_reports_full() {
    let mut page = Page::new_zeroed();
    let mut sp = SlottedPage::new(&mut page);
    sp.init();
    let big = vec![7u8; 4000];
    assert!(sp.insert(0, &big).is_ok());
    assert!(matches!(
        sp.insert(1, &big),
        Err(storage::StorageError::PageFull)
    ));
}
