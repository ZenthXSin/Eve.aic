use eve_llm_api::ImageInput;
use eve_sticker_api::*;
use eve_sticker_plugin::FileStickerLibrary;

/// 最小 PNG：签名 + IHDR，尾部加入不同字节以区分图片。
fn png(tag: u8) -> ImageInput {
    let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x02\0\0\0".to_vec();
    data.push(tag);
    ImageInput::new(data).unwrap()
}
fn approve(moods: &[&str]) -> StickerReview {
    StickerReview {
        sticker: true,
        description: "猫猫点头".into(),
        moods: moods.iter().map(|m| m.to_string()).collect(),
    }
}

#[test]
fn offered_images_are_reviewed_once_and_picked_least_used_first() {
    let dir = tempfile::tempdir().unwrap();
    let library = FileStickerLibrary::open(dir.path()).unwrap();
    let a = library.offer(&png(1), 1).unwrap().unwrap();
    assert_eq!(library.offer(&png(1), 2).unwrap(), None, "同一张图只收一次");
    let b = library.offer(&png(2), 3).unwrap().unwrap();
    let c = library.offer(&png(3), 4).unwrap().unwrap();
    assert!(library.pick("赞同").unwrap().is_none(), "审看前不能发");
    let (first, _) = library.next_pending().unwrap().unwrap();
    assert_eq!(first, a);
    library
        .finish_review(&a, &approve(&["赞同", "开心"]))
        .unwrap();
    library.finish_review(&b, &approve(&["赞同"])).unwrap();
    let photo = library
        .list()
        .unwrap()
        .into_iter()
        .find(|s| s.id == c)
        .unwrap();
    let blob = dir
        .path()
        .join("blobs")
        .join(format!("{}.png", photo.sha256));
    assert!(blob.is_file());
    library
        .finish_review(
            &c,
            &StickerReview {
                sticker: false,
                description: String::new(),
                moods: vec![],
            },
        )
        .unwrap();
    assert!(!blob.exists(), "不是表情包的图片字节被删除");
    assert_eq!(library.offer(&png(3), 5).unwrap(), None, "被拒的图不再收");
    assert!(
        library.finish_review(&a, &approve(&["开心"])).is_err(),
        "不能重复审看"
    );
    assert!(library.finish_review(&b, &approve(&["狂喜"])).is_err());
    let one = library.pick("赞同").unwrap().unwrap();
    let two = library.pick("赞同").unwrap().unwrap();
    assert_ne!(one.id, two.id, "用得少的优先");
    assert!(one.path.is_file());
    assert_eq!(
        library.moods().unwrap(),
        vec!["开心".to_string(), "赞同".to_string()]
    );
    drop(library);
    let reopened = FileStickerLibrary::open(dir.path()).unwrap();
    assert_eq!(
        reopened.list().unwrap().iter().map(|s| s.uses).sum::<u64>(),
        2
    );
    assert!(reopened.forget(&a).unwrap());
    assert!(!reopened.forget(&a).unwrap());
    assert_eq!(reopened.pick("开心").unwrap(), None);
    assert_eq!(
        reopened.offer(&png(1), 6).unwrap(),
        None,
        "删除的图也不再收"
    );
}

#[test]
fn corrupt_index_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let library = FileStickerLibrary::open(dir.path()).unwrap();
    library.offer(&png(9), 1).unwrap().unwrap();
    drop(library);
    let path = dir.path().join("index.json");
    let mut index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    index["stickers"][0]["moods"] = serde_json::json!(["开心"]);
    let bytes = serde_json::to_vec(&index).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        FileStickerLibrary::open(dir.path()),
        Err(StickerError::Corrupt(_))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::write(&path, b"{").unwrap();
    assert!(FileStickerLibrary::open(dir.path()).is_err());
}
