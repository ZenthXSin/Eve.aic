use eve_context_api::*;
use eve_context_plugin::FileContextArchive;
use std::sync::Arc;
fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn parallel_immutable_publication_validates_every_read_and_does_not_repair_corrupt_files() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileContextArchive::open(directory.path()).unwrap());
    let bytes = b"context body bytes";
    let key = hash(bytes);
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (store, key) = (store.clone(), key.clone());
            std::thread::spawn(move || store.put(&key, bytes))
        })
        .collect();
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    assert_eq!(store.get(&key).unwrap(), bytes);
    let path = directory.path().join(format!("{key}.json"));
    std::fs::write(&path, b"bad").unwrap();
    assert_eq!(store.get(&key), Err(ContextError::CorruptState));
    assert_eq!(store.put(&key, bytes), Err(ContextError::CorruptState));
    assert_eq!(std::fs::read(&path).unwrap(), b"bad");
    std::fs::remove_file(&path).unwrap();
    assert_eq!(store.get(&key), Err(ContextError::CorruptState));
}

#[test]
fn traversal_wrong_digest_and_oversized_files_are_rejected_without_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileContextArchive::open(directory.path()).unwrap();
    for key in ["../outside", "", &"A".repeat(64), &"0".repeat(63)] {
        assert_eq!(store.get(key), Err(ContextError::InvalidInput));
        assert_eq!(store.put(key, b"x"), Err(ContextError::InvalidInput));
    }
    assert_eq!(
        store.put(&"0".repeat(64), b"x"),
        Err(ContextError::InvalidInput)
    );
    let large = vec![b'x'; MAX_CONTEXT_BODY_BYTES + 1];
    let key = hash(&large);
    assert_eq!(store.put(&key, &large), Err(ContextError::InvalidInput));
    std::fs::write(directory.path().join(format!("{key}.json")), &large).unwrap();
    assert_eq!(store.get(&key), Err(ContextError::CorruptState));
}

#[cfg(unix)]
#[test]
fn symlink_replacement_is_not_a_valid_cached_body_or_a_writable_target() {
    let directory = tempfile::tempdir().unwrap();
    let store = FileContextArchive::open(directory.path().join("bodies")).unwrap();
    let bytes = b"owned note";
    let key = hash(bytes);
    let target = directory.path().join("outside");
    std::fs::write(&target, bytes).unwrap();
    std::os::unix::fs::symlink(
        &target,
        directory.path().join("bodies").join(format!("{key}.json")),
    )
    .unwrap();
    assert_eq!(store.get(&key), Err(ContextError::CorruptState));
    assert_eq!(store.put(&key, bytes), Err(ContextError::CorruptState));
    assert_eq!(std::fs::read(target).unwrap(), bytes);
}
