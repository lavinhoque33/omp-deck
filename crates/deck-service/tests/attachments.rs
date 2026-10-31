use std::sync::Arc;

use deck_service::attachments::{
    AttachError, AttachmentStore, MAX_FILE, Revocations, sanitize_name,
};

#[test]
fn stores_once_per_content_and_name_below_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::at(dir.path().to_path_buf());
    let a = store.store("dev-1", "shot.png", b"png bytes").unwrap();
    let b = store.store("dev-1", "shot.png", b"png bytes").unwrap();
    assert_eq!(a, b);
    assert!(a.starts_with(dir.path().join("dev-1")));
    assert!(
        a.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("-shot.png")
    );
    assert_eq!(std::fs::read(&a).unwrap(), b"png bytes");
    assert_eq!(store.usage("dev-1").unwrap(), 9);
    store.remove_owner("dev-1").unwrap();
    assert!(!a.exists());
    store.remove_owner("dev-1").unwrap();
}

#[test]
fn a_revoked_owner_never_gets_its_directory_back() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::at(dir.path().to_path_buf());
    let gates = Arc::new(Revocations::default());
    let w = gates.writer("dev-1").unwrap();
    store.store_for(&w, "a.png", b"1").unwrap();
    drop(w);
    // A store in flight when the owner is revoked: marked, the directory removed, then the
    // write lands — it fails and leaves nothing behind.
    let in_flight = gates.writer("dev-1").unwrap();
    let cleanup = gates.revoke("dev-1");
    store.remove_owner("dev-1").unwrap();
    assert!(matches!(
        store.store_for(&in_flight, "b.png", b"2"),
        Err(AttachError::Revoked)
    ));
    assert!(!dir.path().join("dev-1").exists());
    // No new store starts while the revocation is carried out.
    assert!(matches!(gates.writer("dev-1"), Err(AttachError::Revoked)));
    drop(cleanup);
    assert!(
        matches!(gates.writer("dev-1"), Err(AttachError::Revoked)),
        "its writer still runs"
    );
    drop(in_flight);
    assert_eq!(gates.tracked(), 0, "nothing outlives the work in flight");
}

#[test]
fn revocation_marks_last_only_as_long_as_the_work_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::at(dir.path().to_path_buf());
    let gates = Arc::new(Revocations::default());
    // Many pair/revoke cycles without writers leave nothing behind.
    for n in 0..100 {
        let id = format!("dev-{n}");
        let cleanup = gates.revoke(&id);
        store.remove_owner(&id).unwrap();
        drop(cleanup);
    }
    assert_eq!(gates.tracked(), 0);
    // A fresh admission with a revoked id uploads again once the revocation is done.
    let w = gates.writer("dev-7").unwrap();
    assert!(store.store_for(&w, "c.png", b"3").is_ok());
    drop(w);
    assert_eq!(gates.tracked(), 0);
    // Two writers and a removal: the mark lasts until the last of them.
    let (a, b) = (
        gates.writer("dev-x").unwrap(),
        gates.writer("dev-x").unwrap(),
    );
    let cleanup = gates.revoke("dev-x");
    drop(a);
    drop(cleanup);
    assert_eq!(gates.tracked(), 1);
    assert!(matches!(
        store.store_for(&b, "d.png", b"4"),
        Err(AttachError::Revoked)
    ));
    drop(b);
    assert_eq!(gates.tracked(), 0);
    assert!(gates.writer("dev-x").is_ok());
}

#[test]
fn refuses_bad_owners_and_oversized_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::at(dir.path().to_path_buf());
    assert!(matches!(
        store.store("../x", "a", b"1"),
        Err(AttachError::BadOwner)
    ));
    assert!(matches!(
        store.store("", "a", b"1"),
        Err(AttachError::BadOwner)
    ));
    let big = vec![0u8; usize::try_from(MAX_FILE).unwrap() + 1];
    assert!(matches!(
        store.store("dev", "big", &big),
        Err(AttachError::TooLarge)
    ));
}

#[test]
fn names_are_reduced_to_a_safe_leaf() {
    assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_name("C:\\x\\my shot.png"), "my_shot.png");
    assert_eq!(sanitize_name(".hidden"), "hidden");
    assert_eq!(sanitize_name(""), "file");
    assert_eq!(sanitize_name("a".repeat(200).as_str()).len(), 80);
}
