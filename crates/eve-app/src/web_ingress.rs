//! Web 审核原文独立于模型上下文，每个已认证会话仅保留最近六条完整正文。
//! 宿主必须在新 HTTP 请求通过身份、准入和持久回执 claim 后调用 record；
//! 全历史请求去重由 WebChat 回执保证，本库仅对保留窗口内的请求幂等。
//! 首次会话先持久登记再保存正文；登记后缺失、损坏或提交不确定均失败关闭，
//! 不从历史摘要补齐、不清空重建。状态目录属于可信宿主，摘要用于发现存储损坏。
use crate::AppError;
use eve_llm_api::{ContextScope, LlmError, ReviewUserMessageSource};
use eve_session_api::SessionKey;
use eve_web_panel_api::{MAX_CHAT_INPUT_BYTES, valid_chat_id};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

const WINDOW: usize = 6;
// JSON 对合法控制字符最多六倍转义；六条最大正文也必须完整保存。
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SCOPES: usize = 32768;
const MARKER: &[u8] = b"{\"version\":1}";
const UNAVAILABLE: &str = "Web 原始用户正文存储不完整或不可读；未用加工历史代替原话";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    version: u32,
    conversations: BTreeSet<String>,
    fingerprint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMessage {
    request_id: String,
    text: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    scope: ContextScope,
    messages: Vec<RawMessage>,
    fingerprint: String,
}

pub(crate) struct RawMessages {
    root: PathBuf,
    directory: PathBuf,
    claims: PathBuf,
    marker: PathBuf,
    _file_lock: fs::File,
    gate: Mutex<()>,
    failed: AtomicBool,
}

fn unavailable<T>(_: T) -> String {
    UNAVAILABLE.into()
}

fn fingerprint(value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(unavailable)?;
    Ok(digest(&SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_CHAT_INPUT_BYTES && !text.contains('\0')
}

fn read_bound(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(unavailable)?;
    if !metadata.file_type().is_file() || metadata.len() > limit {
        return Err(UNAVAILABLE.into());
    }
    let file = fs::File::open(path).map_err(unavailable)?;
    if !file.metadata().map_err(unavailable)?.is_file() {
        return Err(UNAVAILABLE.into());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > limit {
        return Err(UNAVAILABLE.into());
    }
    Ok(bytes)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(unavailable)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn persist(path: &Path, bytes: &[u8], first: bool) -> Result<(), String> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(UNAVAILABLE.into());
    }
    let parent = path.parent().ok_or_else(|| UNAVAILABLE.to_owned())?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".web-review-pending-")
        .tempfile_in(parent)
        .map_err(unavailable)?;
    temporary.write_all(bytes).map_err(unavailable)?;
    temporary.as_file().sync_all().map_err(unavailable)?;
    if first {
        temporary.persist_noclobber(path).map_err(unavailable)?;
    } else {
        temporary.persist(path).map_err(unavailable)?;
    }
    sync_directory(parent)
}

impl Claims {
    fn fingerprint(&self) -> Result<String, String> {
        fingerprint(&(self.version, &self.conversations))
    }

    fn valid(&self) -> Result<bool, String> {
        Ok(self.version == 1
            && self.conversations.len() <= MAX_SCOPES
            && self.conversations.iter().all(|id| valid_chat_id(id))
            && self.fingerprint == self.fingerprint()?)
    }
}

impl Journal {
    fn fingerprint(&self) -> Result<String, String> {
        fingerprint(&(self.version, &self.scope, &self.messages))
    }

    fn valid(&self, scope: &ContextScope) -> Result<bool, String> {
        let mut requests = BTreeSet::new();
        Ok(self.version == 1
            && &self.scope == scope
            && (1..=WINDOW).contains(&self.messages.len())
            && self.messages.iter().all(|message| {
                valid_chat_id(&message.request_id)
                    && valid_text(&message.text)
                    && requests.insert(&message.request_id)
            })
            && self.fingerprint == self.fingerprint()?)
    }
}

impl RawMessages {
    pub(crate) fn open(root: &Path) -> Result<Self, AppError> {
        Self::open_inner(root).map_err(Into::into)
    }

    fn open_inner(root: &Path) -> Result<Self, String> {
        if !fs::symlink_metadata(root)
            .map_err(unavailable)?
            .file_type()
            .is_dir()
        {
            return Err(UNAVAILABLE.into());
        }
        let root = root.canonicalize().map_err(unavailable)?;
        let directory = root.join("web-review-messages");
        let claims = root.join("web-review-claims.v1.json");
        let marker = root.join("web-review.initialized.v1");
        let initialized = match fs::symlink_metadata(&marker) {
            Ok(_) => {
                if read_bound(&marker, MARKER.len() as u64)? != MARKER {
                    return Err(UNAVAILABLE.into());
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => return Err(UNAVAILABLE.into()),
        };
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                if !initialized
                    && fs::read_dir(&directory)
                        .map_err(unavailable)?
                        .next()
                        .is_some()
                {
                    return Err(UNAVAILABLE.into());
                }
            }
            Err(error) if !initialized && error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&directory).map_err(unavailable)?;
                sync_directory(&root)?;
            }
            _ => return Err(UNAVAILABLE.into()),
        }
        let lock_path = directory.join("lock");
        let file_lock = if initialized {
            if !fs::symlink_metadata(&lock_path)
                .map_err(unavailable)?
                .file_type()
                .is_file()
            {
                return Err(UNAVAILABLE.into());
            }
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(unavailable)?
        } else {
            match fs::symlink_metadata(&claims) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(UNAVAILABLE.into()),
            }
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .map_err(unavailable)?
        };
        file_lock.try_lock().map_err(unavailable)?;
        if !initialized {
            file_lock.sync_all().map_err(unavailable)?;
            sync_directory(&directory)?;
            let mut empty = Claims {
                version: 1,
                conversations: BTreeSet::new(),
                fingerprint: String::new(),
            };
            empty.fingerprint = empty.fingerprint()?;
            persist(
                &claims,
                &serde_json::to_vec(&empty).map_err(unavailable)?,
                true,
            )?;
            persist(&marker, MARKER, true)?;
        }
        let source = Self {
            root,
            directory,
            claims,
            marker,
            _file_lock: file_lock,
            gate: Mutex::new(()),
            failed: AtomicBool::new(false),
        };
        source.validate_inventory(&source.load_claims()?)?;
        Ok(source)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, ()>, String> {
        let guard = self.gate.lock().map_err(unavailable)?;
        if self.failed.load(Ordering::Acquire) {
            return Err(UNAVAILABLE.into());
        }
        Ok(guard)
    }

    fn storage_result<T>(&self, result: Result<T, String>) -> Result<T, String> {
        if result.is_err() {
            self.failed.store(true, Ordering::Release);
        }
        result
    }

    fn path(&self, conversation: &str) -> PathBuf {
        self.directory.join(format!("{conversation}.json"))
    }

    fn load_claims(&self) -> Result<Claims, String> {
        if read_bound(&self.marker, MARKER.len() as u64)? != MARKER {
            return Err(UNAVAILABLE.into());
        }
        for path in [&self.root, &self.directory] {
            if !fs::symlink_metadata(path)
                .map_err(unavailable)?
                .file_type()
                .is_dir()
            {
                return Err(UNAVAILABLE.into());
            }
        }
        let current = fs::symlink_metadata(self.directory.join("lock")).map_err(unavailable)?;
        if !current.file_type().is_file() {
            return Err(UNAVAILABLE.into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let locked = self._file_lock.metadata().map_err(unavailable)?;
            if current.dev() != locked.dev() || current.ino() != locked.ino() {
                return Err(UNAVAILABLE.into());
            }
        }
        let claims: Claims = serde_json::from_slice(&read_bound(&self.claims, MAX_FILE_BYTES)?)
            .map_err(unavailable)?;
        if !claims.valid()? {
            return Err(UNAVAILABLE.into());
        }
        Ok(claims)
    }

    fn load_journal(
        &self,
        claims: &Claims,
        scope: &ContextScope,
        conversation: &str,
    ) -> Result<Option<Journal>, String> {
        let path = self.path(conversation);
        if !claims.conversations.contains(conversation) {
            return match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                _ => Err(UNAVAILABLE.into()),
            };
        }
        let journal: Journal =
            serde_json::from_slice(&read_bound(&path, MAX_FILE_BYTES)?).map_err(unavailable)?;
        if !journal.valid(scope)? {
            return Err(UNAVAILABLE.into());
        }
        Ok(Some(journal))
    }

    fn validate_inventory(&self, claims: &Claims) -> Result<(), String> {
        // 整体重启必须检查每个登记会话，不能把部分丢失当作新会话。
        for conversation in &claims.conversations {
            let scope = ContextScope {
                session_id: format!("{}{conversation}", crate::web_chat::PREFIX),
                user_id: crate::web_chat::USER.into(),
            };
            self.load_journal(claims, &scope, conversation)?;
        }
        for entry in fs::read_dir(&self.directory).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            if !entry.file_type().map_err(unavailable)?.is_file() {
                return Err(UNAVAILABLE.into());
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| UNAVAILABLE.to_owned())?;
            if name == "lock" || name.starts_with(".web-review-pending-") {
                continue;
            }
            let Some(conversation) = name.strip_suffix(".json") else {
                return Err(UNAVAILABLE.into());
            };
            if !claims.conversations.contains(conversation) {
                return Err(UNAVAILABLE.into());
            }
        }
        Ok(())
    }

    pub(crate) fn record(
        &self,
        session: &SessionKey,
        request_id: &str,
        text: &str,
    ) -> Result<(), String> {
        let scope = ContextScope {
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
        };
        if !crate::web_chat::is_owner(&scope) || !valid_chat_id(request_id) || !valid_text(text) {
            return Err("Web 原始用户正文或可信会话范围无效".into());
        }
        let conversation = scope
            .session_id
            .strip_prefix(crate::web_chat::PREFIX)
            .expect("已验证 Web 会话范围");
        let _guard = self.lock()?;
        let claims = self.storage_result(self.load_claims())?;
        let previous = self.storage_result(self.load_journal(&claims, &scope, conversation))?;
        if let Some(message) = previous.as_ref().and_then(|journal| {
            journal
                .messages
                .iter()
                .find(|message| message.request_id == request_id)
        }) {
            return if message.text == text {
                Ok(())
            } else {
                Err("Web 请求编号已有不同原始正文".into())
            };
        }
        let first = previous.is_none();
        if first && claims.conversations.len() == MAX_SCOPES {
            return Err("Web 原始用户正文会话登记已达容量上限".into());
        }
        let mut journal = previous.unwrap_or_else(|| Journal {
            version: 1,
            scope: scope.clone(),
            messages: Vec::new(),
            fingerprint: String::new(),
        });
        journal.messages.push(RawMessage {
            request_id: request_id.into(),
            text: text.into(),
        });
        if journal.messages.len() > WINDOW {
            journal.messages.remove(0);
        }
        self.save_journal(claims, journal, conversation, first)
    }

    fn save_journal(
        &self,
        mut claims: Claims,
        mut journal: Journal,
        conversation: &str,
        first: bool,
    ) -> Result<(), String> {
        self.storage_result((|| {
            journal.fingerprint = journal.fingerprint()?;
            if first {
                claims.conversations.insert(conversation.into());
                claims.fingerprint = claims.fingerprint()?;
                persist(
                    &self.claims,
                    &serde_json::to_vec(&claims).map_err(unavailable)?,
                    false,
                )?;
            }
            persist(
                &self.path(conversation),
                &serde_json::to_vec(&journal).map_err(unavailable)?,
                first,
            )
        })())
    }
}

impl ReviewUserMessageSource for RawMessages {
    fn is_current(&self, scope: &ContextScope, anchor: &str) -> Result<bool, LlmError> {
        if !crate::web_chat::is_owner(scope) || !valid_chat_id(anchor) {
            return Ok(false);
        }
        let _guard = self.lock().map_err(LlmError::Context)?;
        let journal = self
            .storage_result((|| {
                let claims = self.load_claims()?;
                self.load_journal(
                    &claims,
                    scope,
                    scope
                        .session_id
                        .strip_prefix(crate::web_chat::PREFIX)
                        .expect("verified"),
                )
            })())
            .map_err(LlmError::Context)?;
        Ok(journal.is_some_and(|j| j.messages.last().is_some_and(|m| m.request_id == anchor)))
    }
    fn read(&self, scope: &ContextScope) -> Result<Option<Vec<String>>, LlmError> {
        if !crate::web_chat::is_owner(scope) {
            return Ok(None);
        }
        let _guard = self.lock().map_err(LlmError::Context)?;
        self.storage_result((|| {
            let claims = self.load_claims()?;
            let conversation = scope
                .session_id
                .strip_prefix(crate::web_chat::PREFIX)
                .expect("已验证 Web 会话范围");
            Ok(Some(
                self.load_journal(&claims, scope, conversation)?
                    .map(|journal| {
                        journal
                            .messages
                            .into_iter()
                            .map(|message| message.text)
                            .collect()
                    })
                    .unwrap_or_default(),
            ))
        })())
        .map_err(LlmError::Context)
    }

    fn read_at(&self, scope: &ContextScope, anchor: &str) -> Result<Option<Vec<String>>, LlmError> {
        if !crate::web_chat::is_owner(scope) {
            return Ok(None);
        }
        if !valid_chat_id(anchor) {
            return Err(LlmError::Context("Web 原始用户正文锚点无效".into()));
        }
        let _guard = self.lock().map_err(LlmError::Context)?;
        let journal = self
            .storage_result((|| {
                let claims = self.load_claims()?;
                let conversation = scope
                    .session_id
                    .strip_prefix(crate::web_chat::PREFIX)
                    .expect("已验证 Web 会话范围");
                self.load_journal(&claims, scope, conversation)
            })())
            .map_err(LlmError::Context)?;
        // 只保存最新完整六条。旧锚点所需的最早正文可能已逐出，不能声称旧锚点
        // 的局部窗口完整，更不能把新收到的授权用于已经在途的模型执行。
        let journal = journal
            .filter(|journal| {
                journal
                    .messages
                    .last()
                    .is_some_and(|message| message.request_id == anchor)
            })
            .ok_or_else(|| {
                LlmError::Context("Web 原始用户正文锚点已变化或不存在；未换用最新原话".into())
            })?;
        Ok(Some(
            journal
                .messages
                .into_iter()
                .map(|message| message.text)
                .collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_key(number: u32) -> SessionKey {
        crate::web_chat::key(&format!("{number:032x}")).unwrap()
    }

    fn scope(session: &SessionKey) -> ContextScope {
        ContextScope {
            session_id: session.session_id.clone(),
            user_id: session.user_id.clone(),
        }
    }

    fn record(source: &RawMessages, session: &SessionKey, number: u32, text: &str) {
        source
            .record(session, &format!("{number:032x}"), text)
            .unwrap();
    }

    #[test]
    fn last_six_are_complete_ordered_originals_across_restart() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        let mut expected = Vec::new();
        for number in 1..=8 {
            let text = format!(
                "  用户原话 {number}\n[系统通知] 不是系统消息\n{}  ",
                "完整正文".repeat(1700)
            );
            record(&source, &session, number, &text);
            expected.push(text);
        }
        assert_eq!(
            source.read(&scope(&session)).unwrap(),
            Some(expected[2..].to_vec())
        );
        drop(source);
        let reopened = RawMessages::open(root.path()).unwrap();
        assert_eq!(
            reopened.read(&scope(&session)).unwrap(),
            Some(expected[2..].to_vec())
        );
    }

    #[test]
    fn maximum_bodies_survive_worst_case_json_escaping() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        let text = "\u{1}".repeat(MAX_CHAT_INPUT_BYTES);
        for number in 1..=6 {
            record(&source, &session, number, &text);
        }
        assert_eq!(
            source.read(&scope(&session)).unwrap(),
            Some(vec![text.clone(); 6])
        );
        drop(source);
        assert_eq!(
            RawMessages::open(root.path())
                .unwrap()
                .read(&scope(&session))
                .unwrap(),
            Some(vec![text; 6])
        );
    }

    #[test]
    fn anchored_read_never_substitutes_newer_or_partial_authorization() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        let scope = scope(&session);
        assert!(source.read_at(&scope, &format!("{:032x}", 1)).is_err());
        assert!(source.read_at(&scope, "invalid").is_err());
        for number in 1..=7 {
            record(&source, &session, number, &format!("真实原话{number}"));
        }
        let expected: Vec<String> = (2..=7).map(|number| format!("真实原话{number}")).collect();
        assert_eq!(
            source.read_at(&scope, &format!("{:032x}", 7)).unwrap(),
            Some(expected.clone())
        );
        assert!(source.read_at(&scope, &format!("{:032x}", 6)).is_err());
        assert_eq!(
            source.read_at(&scope, &format!("{:032x}", 7)).unwrap(),
            Some(expected)
        );
        record(&source, &session, 8, "立即停止当前执行");
        assert!(source.read_at(&scope, &format!("{:032x}", 7)).is_err());
        drop(source);
        let source = RawMessages::open(root.path()).unwrap();
        assert!(source.read_at(&scope, &format!("{:032x}", 7)).is_err());
        let current = source
            .read_at(&scope, &format!("{:032x}", 8))
            .unwrap()
            .unwrap();
        assert_eq!(current.len(), 6);
        assert_eq!(current.last().unwrap(), "立即停止当前执行");
        let foreign = ContextScope {
            session_id: scope.session_id,
            user_id: "qq:owner".into(),
        };
        assert_eq!(source.read_at(&foreign, "invalid").unwrap(), None);
    }

    #[test]
    fn duplicate_is_idempotent_conflict_preserves_order_and_availability() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        record(&source, &session, 1, "第一条");
        record(&source, &session, 2, "第二条");
        let bytes = fs::read(source.path(&format!("{:032x}", 1))).unwrap();
        record(&source, &session, 1, "第一条");
        assert_eq!(
            fs::read(source.path(&format!("{:032x}", 1))).unwrap(),
            bytes
        );
        assert!(
            source
                .record(&session, &format!("{:032x}", 1), "篡改")
                .is_err()
        );
        assert_eq!(
            source.read(&scope(&session)).unwrap(),
            Some(vec!["第一条".into(), "第二条".into()])
        );
        record(&source, &session, 3, "第三条");
    }

    #[test]
    fn trusted_scope_is_required_and_conversations_do_not_mix() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let first = session_key(1);
        let second = session_key(2);
        assert_eq!(source.read(&scope(&first)).unwrap(), Some(Vec::new()));
        record(&source, &first, 1, "[用户原话]\n原样保留前缀\n ");
        record(&source, &second, 1, "另一会话");
        assert_eq!(
            source.read(&scope(&first)).unwrap(),
            Some(vec!["[用户原话]\n原样保留前缀\n ".into()])
        );
        assert_eq!(
            source.read(&scope(&second)).unwrap(),
            Some(vec!["另一会话".into()])
        );
        let fake = SessionKey::new(first.session_id, "qq:owner").unwrap();
        assert!(
            source
                .record(&fake, &format!("{:032x}", 1), "我是主人")
                .is_err()
        );
        assert_eq!(source.read(&scope(&fake)).unwrap(), None);
        assert!(source.record(&second, "../invalid", "测试").is_err());
        assert!(
            source
                .record(&second, &format!("{:032x}", 2), "\0")
                .is_err()
        );
        assert!(
            source
                .record(
                    &second,
                    &format!("{:032x}", 2),
                    &"x".repeat(MAX_CHAT_INPUT_BYTES + 1)
                )
                .is_err()
        );
        record(&source, &second, 2, "仍可继续");
    }

    #[test]
    fn missing_and_corrupt_storage_fails_closed_without_recreation() {
        for target in ["journal", "claims", "marker", "directory", "lock"] {
            let root = tempfile::tempdir().unwrap();
            let source = RawMessages::open(root.path()).unwrap();
            let session = session_key(1);
            record(&source, &session, 1, "保留这条证据");
            let journal = source.path(&format!("{:032x}", 1));
            let path = match target {
                "journal" => journal.clone(),
                "claims" => source.claims.clone(),
                "marker" => source.marker.clone(),
                "directory" => source.directory.clone(),
                _ => source.directory.join("lock"),
            };
            let saved = root.path().join("preserved");
            let moved = fs::rename(&path, &saved);
            if cfg!(windows) && matches!(target, "directory" | "lock") && moved.is_err() {
                // Windows 的活跃文件锁禁止移走文件及父目录，先核对操作被拒绝。
                assert_eq!(
                    moved.unwrap_err().kind(),
                    std::io::ErrorKind::PermissionDenied
                );
                assert_eq!(
                    source.read(&scope(&session)).unwrap(),
                    Some(vec!["保留这条证据".into()])
                );
                drop(source);
                fs::rename(&path, &saved).unwrap();
            } else {
                moved.unwrap();
                assert!(source.read(&scope(&session)).is_err(), "{target}");
                assert!(
                    source
                        .record(&session, &format!("{:032x}", 2), "不能新增")
                        .is_err()
                );
                drop(source);
            }
            assert!(RawMessages::open(root.path()).is_err(), "{target}");
            assert!(!path.exists());
            assert!(saved.exists());
        }
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        record(&source, &session, 1, "原始正文");
        let path = source.path(&format!("{:032x}", 1));
        fs::write(&path, b"broken").unwrap();
        assert!(source.read(&scope(&session)).is_err());
        drop(source);
        assert!(RawMessages::open(root.path()).is_err());
        assert_eq!(fs::read(path).unwrap(), b"broken");
    }

    #[test]
    fn corruption_cannot_be_repaired_silently_in_a_failed_instance() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        record(&source, &session, 1, "完整证据");
        let path = source.path(&format!("{:032x}", 1));
        let original = fs::read(&path).unwrap();
        fs::write(&source.marker, b"invalid").unwrap();
        assert!(
            source
                .record(&session, &format!("{:032x}", 2), "不可执行")
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::write(&source.marker, MARKER).unwrap();
        assert!(source.read(&scope(&session)).is_err());
        drop(source);
        assert_eq!(
            RawMessages::open(root.path())
                .unwrap()
                .read(&scope(&session))
                .unwrap(),
            Some(vec!["完整证据".into()])
        );
    }

    #[test]
    fn wrong_scope_duplicate_ids_unknown_fields_and_invalid_digest_are_rejected() {
        for mutation in [
            "scope",
            "duplicate",
            "unknown",
            "digest",
            "version",
            "count",
            "over_count",
        ] {
            let root = tempfile::tempdir().unwrap();
            let source = RawMessages::open(root.path()).unwrap();
            let session = session_key(1);
            record(&source, &session, 1, "真实原文");
            let path = source.path(&format!("{:032x}", 1));
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            match mutation {
                "scope" => value["scope"]["session_id"] = format!("web:chat:{:032x}", 2).into(),
                "duplicate" => {
                    value["messages"] =
                        serde_json::json!([value["messages"][0], value["messages"][0]])
                }
                "unknown" => value["authorization"] = true.into(),
                "digest" => value["fingerprint"] = "0".repeat(64).into(),
                "version" => value["version"] = 2.into(),
                "over_count" => value["messages"] = serde_json::Value::Array(
                    (1..=7).map(|number| serde_json::json!({"request_id": format!("{number:032x}"), "text": "原话"})).collect()
                ),
                _ => value["messages"] = serde_json::json!([]),
            }
            // 对结构攻击重算摘要，确认拒绝来自范围/结构约束而不是仅摘要不匹配。
            if !matches!(mutation, "unknown" | "digest") {
                let mut journal: Journal = serde_json::from_value(value).unwrap();
                journal.fingerprint = journal.fingerprint().unwrap();
                value = serde_json::to_value(journal).unwrap();
            }
            fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(source.read(&scope(&session)).is_err(), "{mutation}");
            drop(source);
            assert!(RawMessages::open(root.path()).is_err(), "{mutation}");
        }
    }

    #[test]
    fn invalid_scope_inventory_is_preserved_and_rejected() {
        for mutation in ["unknown", "digest", "version", "id"] {
            let root = tempfile::tempdir().unwrap();
            let source = RawMessages::open(root.path()).unwrap();
            let session = session_key(1);
            record(&source, &session, 1, "完整证据");
            let original = fs::read(source.path(&format!("{:032x}", 1))).unwrap();
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&source.claims).unwrap()).unwrap();
            match mutation {
                "unknown" => value["extra"] = true.into(),
                "digest" => value["fingerprint"] = "0".repeat(64).into(),
                "version" => value["version"] = 2.into(),
                _ => value["conversations"] = serde_json::json!(["../invalid"]),
            }
            if matches!(mutation, "version" | "id") {
                let mut claims: Claims = serde_json::from_value(value).unwrap();
                claims.fingerprint = claims.fingerprint().unwrap();
                value = serde_json::to_value(claims).unwrap();
            }
            let corrupt = serde_json::to_vec(&value).unwrap();
            fs::write(&source.claims, &corrupt).unwrap();
            assert!(source.read(&scope(&session)).is_err(), "{mutation}");
            assert_eq!(
                fs::read(source.path(&format!("{:032x}", 1))).unwrap(),
                original
            );
            let claims_path = source.claims.clone();
            drop(source);
            assert!(RawMessages::open(root.path()).is_err(), "{mutation}");
            assert_eq!(fs::read(claims_path).unwrap(), corrupt);
        }
    }

    #[test]
    fn registered_missing_journal_and_unclaimed_journal_are_not_empty_history() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        record(&source, &session, 1, "缺失也不能重建");
        fs::remove_file(source.path(&format!("{:032x}", 1))).unwrap();
        assert!(source.read(&scope(&session)).is_err());
        drop(source);
        assert!(RawMessages::open(root.path()).is_err());

        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(2);
        fs::write(source.path(&format!("{:032x}", 2)), b"{}").unwrap();
        assert!(source.read(&scope(&session)).is_err());
        drop(source);
        assert!(RawMessages::open(root.path()).is_err());
    }

    #[test]
    fn one_writer_per_root_and_restart_releases_lock() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        assert!(RawMessages::open(root.path()).is_err());
        record(&source, &session_key(1), 1, "唯一写入者");
        drop(source);
        assert!(RawMessages::open(root.path()).is_ok());
    }

    #[test]
    fn failed_atomic_persist_preserves_existing_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::write(&target, b"existing evidence").unwrap();
        assert!(persist(&target, b"must not replace", true).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"existing evidence");
        let blocked_target = root.path().join("directory");
        fs::create_dir(&blocked_target).unwrap();
        fs::write(blocked_target.join("original"), b"preserved").unwrap();
        assert!(persist(&blocked_target, b"rename fails", false).is_err());
        assert_eq!(
            fs::read(blocked_target.join("original")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn failed_journal_commit_keeps_original_evidence_and_closes_instance() {
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        record(&source, &session, 1, "提交前的原始证据");
        let conversation = format!("{:032x}", 1);
        let claims = source.load_claims().unwrap();
        let mut journal = source
            .load_journal(&claims, &scope(&session), &conversation)
            .unwrap()
            .unwrap();
        journal.messages.push(RawMessage {
            request_id: format!("{:032x}", 2),
            text: "未提交的新原话".into(),
        });
        let path = source.path(&conversation);
        let original = fs::read(&path).unwrap();
        let preserved = root.path().join("preserved-original");
        fs::rename(&path, &preserved).unwrap();
        fs::create_dir(&path).unwrap();
        // 在 record 使用的生产保存边界制造真实 rename 失败，而不是读取预检失败。
        assert!(
            source
                .save_journal(claims, journal, &conversation, false)
                .is_err()
        );
        assert_eq!(fs::read(&preserved).unwrap(), original);
        fs::remove_dir(&path).unwrap();
        fs::rename(&preserved, &path).unwrap();
        assert!(source.read(&scope(&session)).is_err());
        assert!(
            source
                .record(&session, &format!("{:032x}", 2), "不可恢复写入")
                .is_err()
        );
        drop(source);
        assert_eq!(
            RawMessages::open(root.path())
                .unwrap()
                .read(&scope(&session))
                .unwrap(),
            Some(vec!["提交前的原始证据".into()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_evidence_and_replaced_lock_fail_closed() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        let session = session_key(1);
        record(&source, &session, 1, "可信正文");
        let path = source.path(&format!("{:032x}", 1));
        let original = root.path().join("original");
        fs::rename(&path, &original).unwrap();
        symlink(&original, &path).unwrap();
        assert!(source.read(&scope(&session)).is_err());
        drop(source);
        assert!(RawMessages::open(root.path()).is_err());

        let root = tempfile::tempdir().unwrap();
        let source = RawMessages::open(root.path()).unwrap();
        record(&source, &session, 1, "锁保护正文");
        fs::rename(source.directory.join("lock"), root.path().join("old-lock")).unwrap();
        fs::write(source.directory.join("lock"), b"").unwrap();
        assert!(source.read(&scope(&session)).is_err());
    }
}
