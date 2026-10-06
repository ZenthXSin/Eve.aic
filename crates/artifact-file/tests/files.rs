use eve_action_api::{ArtifactError, ArtifactTarget, MAX_ARTIFACT_BYTES};
use eve_artifact_file::BoundArtifactFile;
use std::{fs, path::Path};
use tempfile::tempdir;

#[test]
fn new_artifact_has_an_independently_read_receipt() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("result.txt");
    let target = BoundArtifactFile::bind(&path).unwrap();
    let adapter: &dyn ArtifactTarget = &target;
    adapter.write_new(b"hello").unwrap();
    let receipt = adapter.read_back(42).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"hello");
    assert_eq!(receipt.artifact_source_id, target.source_id());
    assert_eq!(receipt.byte_count, 5);
    assert_eq!(receipt.verified_at_ms, 42);
    assert_eq!(
        receipt.sha256,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
    receipt.validate().unwrap();

    // 外部改动发生在独立回读之前：不能继续返回原写入缓冲区的摘要。
    fs::write(&path, b"changed").unwrap();
    let changed = adapter.read_back(43).unwrap();
    assert_eq!(changed.byte_count, 7);
    assert_ne!(changed.sha256, receipt.sha256);
}

#[test]
fn preexisting_file_and_second_write_are_never_overwritten() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("existing.txt");
    fs::write(&path, b"original").unwrap();
    let target = BoundArtifactFile::bind(&path).unwrap();
    assert_eq!(
        target.write_new(b"replacement"),
        Err(ArtifactError::AlreadyExists)
    );
    assert_eq!(fs::read(&path).unwrap(), b"original");

    let fresh_path = directory.path().join("fresh.txt");
    let fresh = BoundArtifactFile::bind(&fresh_path).unwrap();
    fresh.write_new(b"first").unwrap();
    assert_eq!(
        fresh.write_new(b"second"),
        Err(ArtifactError::AlreadyExists)
    );
    assert_eq!(fs::read(fresh_path).unwrap(), b"first");
}

#[test]
fn invalid_text_and_oversized_text_do_not_create_a_file() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("result.txt");
    let target = BoundArtifactFile::bind(&path).unwrap();
    assert_eq!(
        target.write_new(b"contains\0nul"),
        Err(ArtifactError::InvalidInput)
    );
    assert_eq!(target.write_new(&[0xff]), Err(ArtifactError::InvalidInput));
    assert_eq!(
        target.write_new(&vec![b'x'; MAX_ARTIFACT_BYTES as usize + 1]),
        Err(ArtifactError::LimitReached)
    );
    assert!(!path.exists());
}

#[test]
fn empty_and_exact_limit_text_are_accepted() {
    let directory = tempdir().unwrap();
    let empty = BoundArtifactFile::bind(&directory.path().join("empty.txt")).unwrap();
    empty.write_new(b"").unwrap();
    assert_eq!(empty.read_back(1).unwrap().byte_count, 0);

    let full = BoundArtifactFile::bind(&directory.path().join("full.txt")).unwrap();
    full.write_new(&vec![b'x'; MAX_ARTIFACT_BYTES as usize])
        .unwrap();
    assert_eq!(full.read_back(1).unwrap().byte_count, MAX_ARTIFACT_BYTES);

    let unicode = BoundArtifactFile::bind(&directory.path().join("unicode.txt")).unwrap();
    unicode.write_new("本地输出\n".as_bytes()).unwrap();
    assert_eq!(unicode.read_back(1).unwrap().byte_count, 13);
}

#[test]
fn external_invalid_or_large_output_cannot_produce_a_receipt() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("result.txt");
    let target = BoundArtifactFile::bind(&path).unwrap();
    assert!(matches!(target.read_back(1), Err(ArtifactError::NotFound)));
    fs::write(&path, b"good").unwrap();
    assert!(matches!(
        target.read_back(0),
        Err(ArtifactError::InvalidInput)
    ));
    fs::write(&path, b"bad\0text").unwrap();
    assert!(matches!(
        target.read_back(1),
        Err(ArtifactError::InvalidInput)
    ));
    fs::write(&path, [0xff]).unwrap();
    assert!(matches!(
        target.read_back(1),
        Err(ArtifactError::InvalidInput)
    ));
    fs::write(&path, vec![b'x'; MAX_ARTIFACT_BYTES as usize + 1]).unwrap();
    assert!(matches!(
        target.read_back(1),
        Err(ArtifactError::LimitReached)
    ));
    assert_eq!(fs::metadata(path).unwrap().len(), MAX_ARTIFACT_BYTES + 1);
}

#[test]
fn bind_requires_an_existing_parent_and_a_file_name() {
    let directory = tempdir().unwrap();
    let missing_parent = directory.path().join("missing");
    assert!(matches!(
        BoundArtifactFile::bind(&missing_parent.join("result.txt")),
        Err(ArtifactError::NotFound)
    ));
    assert!(!missing_parent.exists());
    assert!(matches!(
        BoundArtifactFile::bind(Path::new(".")),
        Err(ArtifactError::InvalidInput)
    ));
    assert!(matches!(
        BoundArtifactFile::bind(Path::new("..")),
        Err(ArtifactError::InvalidInput)
    ));
    assert!(matches!(
        BoundArtifactFile::bind(directory.path()),
        Err(ArtifactError::InvalidInput)
    ));
    assert!(matches!(
        BoundArtifactFile::bind(&directory.path().join("output.txt/.")),
        Err(ArtifactError::InvalidInput)
    ));
}

#[test]
fn source_is_stable_for_canonical_parent_and_redacts_paths() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("private-result.txt");
    let target = BoundArtifactFile::bind(&path).unwrap();
    let normalized =
        BoundArtifactFile::bind(&directory.path().join("./private-result.txt")).unwrap();
    let other = BoundArtifactFile::bind(&directory.path().join("another.txt")).unwrap();
    assert_eq!(target.source_id(), normalized.source_id());
    assert_ne!(target.source_id(), other.source_id());
    assert!(target.source_id().starts_with("artifact-file:"));
    let hash = target.source_id().strip_prefix("artifact-file:").unwrap();
    assert_eq!(hash.len(), 64);
    assert!(
        hash.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(format!("{target:?}"), "BoundArtifactFile(<redacted>)");
    assert!(!format!("{target:?}").contains("private-result"));
}

#[cfg(unix)]
#[test]
fn new_output_is_private_to_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempdir().unwrap();
    let path = directory.path().join("private.txt");
    BoundArtifactFile::bind(&path)
        .unwrap()
        .write_new(b"secret")
        .unwrap();
    assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
}

#[cfg(unix)]
#[test]
fn final_symlinks_are_rejected_without_modifying_their_target() {
    use std::os::unix::fs::symlink;
    let directory = tempdir().unwrap();
    let original = directory.path().join("original.txt");
    let path = directory.path().join("output.txt");
    fs::write(&original, b"original").unwrap();
    let bound = BoundArtifactFile::bind(&path).unwrap();
    symlink(&original, &path).unwrap();
    assert!(matches!(
        BoundArtifactFile::bind(&path),
        Err(ArtifactError::InvalidInput)
    ));
    assert_eq!(
        bound.write_new(b"replacement"),
        Err(ArtifactError::AlreadyExists)
    );
    assert!(matches!(
        bound.read_back(1),
        Err(ArtifactError::InvalidInput)
    ));
    assert_eq!(fs::read(original).unwrap(), b"original");
}

#[cfg(unix)]
#[test]
fn explicit_parent_symlink_is_canonicalized_before_binding() {
    use std::os::unix::fs::symlink;
    let directory = tempdir().unwrap();
    let real = directory.path().join("real");
    let alias = directory.path().join("alias");
    fs::create_dir(&real).unwrap();
    symlink(&real, &alias).unwrap();
    let target = BoundArtifactFile::bind(&real.join("output.txt")).unwrap();
    let via_alias = BoundArtifactFile::bind(&alias.join("output.txt")).unwrap();
    assert_eq!(target.source_id(), via_alias.source_id());
    via_alias.write_new(b"output").unwrap();
    assert_eq!(fs::read(real.join("output.txt")).unwrap(), b"output");
}

#[cfg(unix)]
#[test]
fn fifo_cannot_be_bound_or_read_and_is_not_opened_for_writing() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let directory = tempdir().unwrap();
    let path = directory.path().join("output.txt");
    let bound = BoundArtifactFile::bind(&path).unwrap();
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    // 创建本测试临时目录内的 FIFO；不打开或连接任何进程。
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        BoundArtifactFile::bind(&path),
        Err(ArtifactError::InvalidInput)
    ));
    assert!(matches!(
        bound.read_back(1),
        Err(ArtifactError::InvalidInput)
    ));
    assert_eq!(
        bound.write_new(b"no writer"),
        Err(ArtifactError::AlreadyExists)
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_file_names_are_supported_without_exposing_them() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let directory = tempdir().unwrap();
    let name = OsString::from_vec(vec![b'o', 0xff, b't']);
    let path = directory.path().join(name);
    let target = BoundArtifactFile::bind(&path).unwrap();
    target.write_new(b"text").unwrap();
    assert_eq!(target.read_back(1).unwrap().byte_count, 4);
    assert_eq!(format!("{target:?}"), "BoundArtifactFile(<redacted>)");
}
