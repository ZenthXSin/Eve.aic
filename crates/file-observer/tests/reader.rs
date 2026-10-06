use eve_file_observer::{BoundFileObserver, FileReadError};
use std::fs;

#[test]
fn reads_real_bytes_with_stable_source_and_no_path_in_observation_or_debug() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("private-source.txt");
    fs::write(&path, "abc").unwrap();
    let observer = BoundFileObserver::bind(&path).unwrap();
    let first = observer.read_input("goal", 1, 100).unwrap();
    assert_eq!(
        first.sha256,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(first.byte_count, 3);
    assert_eq!(first.text_excerpt, "abc");
    assert!(!first.text_truncated);
    let alternate = BoundFileObserver::bind(&root.path().join("./private-source.txt")).unwrap();
    assert_eq!(alternate.source_id(), observer.source_id());
    assert_eq!(first.observation_source_id, observer.source_id());
    for output in [first.to_json().unwrap(), format!("{observer:?} {first:?}")] {
        assert!(!output.contains(path.to_str().unwrap()));
        assert!(!output.contains("private-source.txt"));
    }
    fs::write(&path, "").unwrap();
    let empty = observer.read_input("goal", 2, 101).unwrap();
    assert_eq!(empty.byte_count, 0);
    assert_eq!(empty.text_excerpt, "");
    assert!(!empty.text_truncated);
    assert_ne!(empty.sha256, first.sha256);
    assert_eq!(empty.observation_source_id, first.observation_source_id);
}

#[test]
fn bounds_full_read_and_utf8_excerpt_without_corrupting_multibyte_text() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    let text = format!("{}{}", "文".repeat(21_845), "x");
    assert_eq!(text.len(), 65_536);
    fs::write(&path, &text).unwrap();
    let observer = BoundFileObserver::bind(&path).unwrap();
    let first = observer.read_input("goal", 1, 100).unwrap();
    assert_eq!(first.byte_count, 65_536);
    assert!(first.text_truncated);
    assert!(first.text_excerpt.len() <= 4096);
    assert!(text.starts_with(&first.text_excerpt));
    assert!(!first.text_excerpt.is_empty());
    assert!(first.to_json().unwrap().len() <= 8192);
    fs::write(&path, "x".repeat(65_537)).unwrap();
    assert_eq!(
        observer.read_input("goal", 1, 101),
        Err(FileReadError::TooLarge)
    );
    assert!(matches!(
        BoundFileObserver::bind(&path),
        Err(FileReadError::TooLarge)
    ));
}

#[test]
fn escaped_control_characters_fit_serialized_contract_and_invalid_text_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    fs::write(&path, "\u{0001}".repeat(4096)).unwrap();
    let observer = BoundFileObserver::bind(&path).unwrap();
    let read = observer.read_input("goal", 1, 100).unwrap();
    assert_eq!(read.byte_count, 4096);
    assert!(read.text_truncated);
    assert!(serde_json::to_string(&read.text_excerpt).unwrap().len() <= 4096);
    assert!(read.to_json().unwrap().len() <= 8192);
    for bytes in [&[0xff, 0xfe][..], b"a\0b"] {
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            observer.read_input("goal", 1, 100),
            Err(FileReadError::InvalidText)
        );
    }
}

#[test]
fn follows_explicit_path_through_atomic_replacement_and_reports_missing_file() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    fs::write(&path, "before").unwrap();
    let observer = BoundFileObserver::bind(&path).unwrap();
    let before = observer.read_input("goal", 1, 100).unwrap();
    let replacement = root.path().join("next.txt");
    fs::write(&replacement, "after").unwrap();
    fs::rename(&replacement, &path).unwrap();
    let after = observer.read_input("goal", 2, 101).unwrap();
    assert_eq!(after.text_excerpt, "after");
    assert_ne!(after.sha256, before.sha256);
    assert_eq!(after.observation_source_id, before.observation_source_id);
    fs::remove_file(&path).unwrap();
    let error = observer.read_input("goal", 3, 102).unwrap_err();
    assert_eq!(error, FileReadError::Io(std::io::ErrorKind::NotFound));
    assert!(!error.to_string().contains(path.to_str().unwrap()));
    assert!(matches!(
        BoundFileObserver::bind(root.path()),
        Err(FileReadError::NotRegular)
    ));
}

#[cfg(unix)]
#[test]
fn rejects_replacement_symlink_and_fifo_without_reading_the_target() {
    use std::{ffi::CString, os::unix::fs::symlink};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    fs::write(&path, "allowed").unwrap();
    let observer = BoundFileObserver::bind(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let other = root.path().join("other-private.txt");
    fs::write(&other, "must not be read").unwrap();
    symlink(&other, &path).unwrap();
    assert_eq!(
        observer.read_input("goal", 1, 100),
        Err(FileReadError::NotRegular)
    );
    fs::remove_file(&path).unwrap();
    let cpath = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // 创建的 FIFO 仅位于此测试的临时目录，不启动或终止任何外部服务。
    assert_eq!(unsafe { libc::mkfifo(cpath.as_ptr(), 0o600) }, 0);
    assert_eq!(
        observer.read_input("goal", 1, 100),
        Err(FileReadError::NotRegular)
    );
    assert!(matches!(
        BoundFileObserver::bind(&path),
        Err(FileReadError::NotRegular)
    ));
}
