//! 文件目录里的表情包库：`index.json` 记录每张图的状态，字节存在 `blobs/` 下。
//!
//! 索引先写临时文件、同步后原子改名。收下新图时先写字节再写索引；删除时先写索引再删字节，
//! 中途退出只会留下没有索引的孤立字节文件，不会出现索引指向不存在的字节。损坏或版本不兼容的
//! 索引拒绝打开，原文件保留。
use eve_llm_api::ImageInput;
use eve_sticker_api::*;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
};
mod reviewer;
pub use reviewer::ModelStickerReviewer;

const FORMAT_VERSION: u32 = 1;
const INDEX: &str = "index.json";
const MAX_INDEX_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DESCRIPTION_CHARS: usize = 60;
const MAX_MOODS: usize = 3;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format_version: u32,
    next: u64,
    stickers: Vec<Sticker>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn extension(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}
fn corrupt() -> StickerError {
    StickerError::Corrupt("表情包索引损坏或版本不兼容；未清空".into())
}
fn storage() -> StickerError {
    StickerError::Storage("表情包库读写失败；保留原状态".into())
}

/// 审看结论的形状：描述非空且不超长，心情来自固定列表、不重复。
pub fn valid_review(review: &StickerReview) -> bool {
    let mut seen = BTreeSet::new();
    let description = review.description.chars().count();
    if !review.sticker {
        return description <= MAX_DESCRIPTION_CHARS && review.moods.is_empty();
    }
    (1..=MAX_DESCRIPTION_CHARS).contains(&description)
        && review.description.trim() == review.description
        && !review.description.chars().any(char::is_control)
        && (1..=MAX_MOODS).contains(&review.moods.len())
        && review.moods.iter().all(|m| valid_mood(m) && seen.insert(m))
}

pub struct FileStickerLibrary {
    directory: PathBuf,
    index: Mutex<Index>,
}

impl FileStickerLibrary {
    pub fn open(directory: impl AsRef<Path>) -> StickerResult<Self> {
        fs::create_dir_all(directory.as_ref().join("blobs")).map_err(|_| storage())?;
        let directory = directory.as_ref().canonicalize().map_err(|_| storage())?;
        let path = directory.join(INDEX);
        let index = match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Index {
                format_version: FORMAT_VERSION,
                next: 0,
                stickers: vec![],
            },
            Err(_) => return Err(storage()),
            Ok(meta) if !meta.is_file() || meta.len() > MAX_INDEX_BYTES => return Err(corrupt()),
            Ok(_) => {
                let bytes = fs::read(&path).map_err(|_| storage())?;
                let index: Index = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
                Self::validate(&index)?;
                index
            }
        };
        Ok(Self {
            directory,
            index: Mutex::new(index),
        })
    }

    fn validate(index: &Index) -> StickerResult<()> {
        let mut ids = BTreeSet::new();
        let mut hashes = BTreeSet::new();
        let pending = index
            .stickers
            .iter()
            .filter(|s| s.state == StickerState::Pending)
            .count();
        let kept = index
            .stickers
            .iter()
            .filter(|s| s.state == StickerState::Approved)
            .count();
        let bad = index.stickers.iter().any(|s| {
            let numbered =
                s.id.strip_prefix('s')
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some_and(|n| n > 0 && n <= index.next && s.id == format!("s{n}"));
            let reviewed = StickerReview {
                sticker: s.state == StickerState::Approved,
                description: s.description.clone(),
                moods: s.moods.clone(),
            };
            !numbered
                || !ids.insert(&s.id)
                || !hashes.insert(&s.sha256)
                || s.sha256.len() != 64
                || !s
                    .sha256
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                || extension(&s.media_type).is_none()
                || s.bytes == 0
                || s.bytes > MAX_STICKER_BYTES
                || match s.state {
                    StickerState::Pending => !s.description.is_empty() || !s.moods.is_empty(),
                    _ => !valid_review(&reviewed),
                }
        });
        if index.format_version != FORMAT_VERSION
            || bad
            || pending > MAX_PENDING
            || kept > MAX_STICKERS
            || index.stickers.len() > MAX_STICKERS * 4
        {
            return Err(corrupt());
        }
        Ok(())
    }

    fn blob(&self, sticker: &Sticker) -> PathBuf {
        let ext = extension(&sticker.media_type).unwrap_or("bin");
        self.directory
            .join("blobs")
            .join(format!("{}.{ext}", sticker.sha256))
    }

    fn write_file(path: &Path, bytes: &[u8]) -> StickerResult<()> {
        let directory = path.parent().ok_or_else(storage)?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(storage)?;
        let temporary = directory.join(format!(".{name}.{}.tmp", std::process::id()));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
            return Err(storage());
        }
        Ok(())
    }

    fn save(&self, index: &Index) -> StickerResult<()> {
        let bytes = serde_json::to_vec(index).map_err(|_| storage())?;
        Self::write_file(&self.directory.join(INDEX), &bytes)
    }
}

impl StickerLibrary for FileStickerLibrary {
    fn offer(&self, image: &ImageInput, now_ms: u64) -> StickerResult<Option<String>> {
        let data = image.data();
        if data.is_empty() || data.len() > MAX_STICKER_BYTES {
            return Ok(None);
        }
        let mut index = self.index.lock().map_err(|_| storage())?;
        let sha256 = hex(digest(&SHA256, data).as_ref());
        let pending = index
            .stickers
            .iter()
            .filter(|s| s.state == StickerState::Pending)
            .count();
        let kept = index
            .stickers
            .iter()
            .filter(|s| s.state == StickerState::Approved)
            .count();
        if index.stickers.iter().any(|s| s.sha256 == sha256)
            || pending >= MAX_PENDING
            || kept >= MAX_STICKERS
            || index.stickers.len() >= MAX_STICKERS * 4
        {
            return Ok(None);
        }
        let mut next = index.clone();
        next.next += 1;
        let sticker = Sticker {
            id: format!("s{}", next.next),
            sha256,
            media_type: image.media_type().into(),
            bytes: data.len(),
            state: StickerState::Pending,
            description: String::new(),
            moods: vec![],
            uses: 0,
            added_at_ms: now_ms,
        };
        // 先写字节再写索引：中途退出只留下孤立字节。
        Self::write_file(&self.blob(&sticker), data)?;
        let id = sticker.id.clone();
        next.stickers.push(sticker);
        self.save(&next)?;
        *index = next;
        Ok(Some(id))
    }

    fn next_pending(&self) -> StickerResult<Option<(String, ImageInput)>> {
        let index = self.index.lock().map_err(|_| storage())?;
        let Some(sticker) = index
            .stickers
            .iter()
            .find(|s| s.state == StickerState::Pending)
        else {
            return Ok(None);
        };
        let bytes = fs::read(self.blob(sticker)).map_err(|_| storage())?;
        let image = ImageInput::new(bytes).map_err(|_| storage())?;
        Ok(Some((sticker.id.clone(), image)))
    }

    fn finish_review(&self, id: &str, review: &StickerReview) -> StickerResult<()> {
        if !valid_review(review) {
            return Err(StickerError::Invalid("表情包审看结果不合格".into()));
        }
        let mut index = self.index.lock().map_err(|_| storage())?;
        let mut next = index.clone();
        let sticker = next
            .stickers
            .iter_mut()
            .find(|s| s.id == id && s.state == StickerState::Pending)
            .ok_or_else(|| StickerError::Invalid("没有这张待审看的图片".into()))?;
        sticker.state = if review.sticker {
            StickerState::Approved
        } else {
            StickerState::Rejected
        };
        sticker.description = if review.sticker {
            review.description.clone()
        } else {
            String::new()
        };
        sticker.moods = review.moods.clone();
        let rejected = (!review.sticker).then(|| self.blob(sticker));
        self.save(&next)?;
        *index = next;
        // 先写索引再删字节；删不掉只留孤立文件。
        if let Some(path) = rejected {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    fn pick(&self, mood: &str) -> StickerResult<Option<StickerChoice>> {
        let mut index = self.index.lock().map_err(|_| storage())?;
        let mut next = index.clone();
        let Some(sticker) = next
            .stickers
            .iter_mut()
            .filter(|s| s.state == StickerState::Approved && s.moods.iter().any(|m| m == mood))
            .min_by(|a, b| a.uses.cmp(&b.uses).then(a.added_at_ms.cmp(&b.added_at_ms)))
        else {
            return Ok(None);
        };
        sticker.uses = sticker.uses.saturating_add(1);
        let choice = StickerChoice {
            id: sticker.id.clone(),
            path: self.blob(sticker),
            description: sticker.description.clone(),
        };
        if !choice.path.is_file() {
            return Err(storage());
        }
        self.save(&next)?;
        *index = next;
        Ok(Some(choice))
    }

    fn moods(&self) -> StickerResult<Vec<String>> {
        let index = self.index.lock().map_err(|_| storage())?;
        Ok(MOODS
            .iter()
            .filter(|mood| {
                index.stickers.iter().any(|s| {
                    s.state == StickerState::Approved && s.moods.iter().any(|m| m == *mood)
                })
            })
            .map(|m| (*m).to_owned())
            .collect())
    }

    fn list(&self) -> StickerResult<Vec<Sticker>> {
        Ok(self.index.lock().map_err(|_| storage())?.stickers.clone())
    }

    fn forget(&self, id: &str) -> StickerResult<bool> {
        let mut index = self.index.lock().map_err(|_| storage())?;
        let Some(position) = index
            .stickers
            .iter()
            .position(|s| s.id == id && s.state != StickerState::Rejected)
        else {
            return Ok(false);
        };
        let mut next = index.clone();
        // 删除后留一条“已拒绝”记录：同一张图以后不再收。
        let sticker = &mut next.stickers[position];
        let path = self.blob(sticker);
        sticker.state = StickerState::Rejected;
        sticker.description.clear();
        sticker.moods.clear();
        self.save(&next)?;
        *index = next;
        let _ = fs::remove_file(path);
        Ok(true)
    }
}
