//! Web 主动通知的持久收件箱。先保存正文和待结算事件，再写任务/触发器本地回执。
//! 浏览器不参与采集；已收录不表示用户阅读或 QQ 平台发送。
use eve_session_api::SessionKey;
use eve_task_api::{Task, TaskAdmin, TaskOwner, TaskStatus};
use eve_trigger_api::{Status as TriggerStatus, TriggerAdmin};
use eve_web_panel_api::{Notification, NotificationPage, PanelError, PanelResult, WebInbox};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_ITEMS: usize = 2048;
const MAX_EVENTS: usize = 65_536;
const MAX_BYTES: u64 = 32 * 1024 * 1024;
// 新收录预留所有已读时间及结算字段的最坏增长；满库仍能确认已读以释放容量。
const RECEIPT_RESERVE_BYTES: u64 = MAX_ITEMS as u64 * 64;
const MAX_SAFE_ID: u64 = 9_007_199_254_740_991;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Seen {
    source: String,
    kind: String,
    events: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Settlement {
    Task {
        owner: TaskOwner,
        id: String,
        source: String,
        created_at_ms: u64,
        finished_at_ms: u64,
    },
    Progress {
        owner: TaskOwner,
        id: String,
        source: String,
        created_at_ms: u64,
        sequence: u64,
        at_ms: u64,
    },
    Trigger {
        owner: SessionKey,
        id: String,
        source: String,
        created_at_ms: u64,
        run: u32,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    source: String,
    event: String,
    notification: Notification,
    pending: Option<Settlement>,
    /// recorded 为账本已结算；superseded 为已有新的执行周期，旧正文仍保留。
    settlement: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    next_id: u64,
    seen: Vec<Seen>,
    records: Vec<Record>,
}
struct Inner {
    ledger: Ledger,
    available: bool,
    initialized: bool,
}
pub(crate) struct Inbox {
    directory: PathBuf,
    // 实例独占锁随句柄释放；两个宿主不能覆盖同一收件箱文件。
    _lock: DirectoryLock,
    inner: Mutex<Inner>,
}
struct DirectoryLock(fs::File);
impl Drop for DirectoryLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn unavailable<T>(_: T) -> PanelError {
    PanelError::Unavailable
}
fn hash(value: &impl Serialize) -> String {
    let bytes = serde_json::to_vec(value).expect("通知身份仅包含可序列化的整数和字符串");
    digest(&SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn web_owner(session: &str, user: &str) -> bool {
    crate::web_chat::is_owner(&eve_llm_api::ContextScope {
        session_id: session.into(),
        user_id: user.into(),
    })
}
fn source_key(session: &str, user: &str, id: &str, created: u64, kind: &str) -> String {
    hash(&(kind, session, user, id, created))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate(ledger: &Ledger) -> PanelResult<()> {
    if ledger.version != 1
        || ledger.next_id == 0
        || ledger.next_id > MAX_SAFE_ID
        || ledger.records.len() > MAX_ITEMS
        || ledger.seen.len() > eve_task_api::MAX_TASKS + eve_trigger_api::MAX_TRIGGERS
    {
        return Err(PanelError::Unavailable);
    }
    let mut sources = BTreeSet::new();
    let mut events = BTreeSet::new();
    for seen in &ledger.seen {
        if !valid_hash(&seen.source)
            || !sources.insert(&seen.source)
            || seen.events.is_empty()
            || !matches!(seen.kind.as_str(), "task" | "trigger")
        {
            return Err(PanelError::Unavailable);
        }
        for event in &seen.events {
            if !valid_hash(event) || !events.insert(event) || events.len() > MAX_EVENTS {
                return Err(PanelError::Unavailable);
            }
        }
    }
    let mut previous = 0;
    let mut record_events = BTreeSet::new();
    for record in &ledger.records {
        let n = &record.notification;
        if n.id <= previous
            || n.id >= ledger.next_id
            || !valid_hash(&record.source)
            || !valid_hash(&record.event)
            || !record_events.insert(&record.event)
            || !web_owner(&n.session_id, crate::web_chat::USER)
            || eve_task_api::validate_id(&n.source_id).is_err()
            || !matches!(n.kind.as_str(), "task" | "trigger" | "progress")
            || !matches!(
                n.status.as_str(),
                "queued"
                    | "running"
                    | "completed"
                    | "blocked"
                    | "failed"
                    | "cancelled"
                    | "interrupted"
                    | "expired"
            )
            || n.title.trim().is_empty()
            || n.title.len() > 160
            || n.body.len() > 8192
            || n.created_at_ms == 0
            || n.read_at_ms.is_some_and(|at| at < n.created_at_ms)
            || record
                .settlement
                .as_deref()
                .is_some_and(|s| !matches!(s, "recorded" | "superseded"))
            || (record.pending.is_some() && record.settlement.is_some())
            || ledger.seen.iter().any(|seen| {
                seen.source == record.source
                    && (!seen.events.contains(&record.event)
                        || (seen.kind == "trigger") != (n.kind == "trigger"))
            })
        {
            return Err(PanelError::Unavailable);
        }
        if let Some(pending) = &record.pending {
            let (session, user, id, source, created, kind) = match pending {
                Settlement::Task {
                    owner,
                    id,
                    source,
                    created_at_ms,
                    finished_at_ms,
                } => {
                    let status = match n.status.as_str() {
                        "completed" => TaskStatus::Completed,
                        "blocked" => TaskStatus::Blocked,
                        "failed" => TaskStatus::Failed,
                        "interrupted" => TaskStatus::Interrupted,
                        _ => return Err(PanelError::Unavailable),
                    };
                    if *finished_at_ms < *created_at_ms
                        || n.kind != "task"
                        || record.event != hash(&(&record.source, *finished_at_ms, status))
                    {
                        return Err(PanelError::Unavailable);
                    }
                    (
                        &owner.session_id,
                        &owner.user_id,
                        id,
                        source,
                        *created_at_ms,
                        "task",
                    )
                }
                Settlement::Progress {
                    owner,
                    id,
                    source,
                    created_at_ms,
                    sequence,
                    at_ms,
                } => {
                    if *sequence == 0
                        || *at_ms < *created_at_ms
                        || n.kind != "progress"
                        || !matches!(n.status.as_str(), "queued" | "running" | "blocked")
                        || n.created_at_ms != *at_ms
                        || record.event != hash(&(&record.source, "progress", *sequence))
                    {
                        return Err(PanelError::Unavailable);
                    }
                    (
                        &owner.session_id,
                        &owner.user_id,
                        id,
                        source,
                        *created_at_ms,
                        "task",
                    )
                }
                Settlement::Trigger {
                    owner,
                    id,
                    source,
                    created_at_ms,
                    run,
                } => {
                    if n.kind != "trigger"
                        || !matches!(n.status.as_str(), "completed" | "failed" | "interrupted")
                        || record.event != hash(&(&record.source, *run, n.status.as_str()))
                    {
                        return Err(PanelError::Unavailable);
                    }
                    (
                        &owner.session_id,
                        &owner.user_id,
                        id,
                        source,
                        *created_at_ms,
                        "trigger",
                    )
                }
            };
            if created == 0
                || !web_owner(session, user)
                || *id != n.source_id
                || *session != n.session_id
                || eve_task_api::validate_id(source).is_err()
                || record.source != source_key(session, user, id, created, kind)
                || !ledger
                    .seen
                    .iter()
                    .any(|s| s.source == record.source && s.events.contains(&record.event))
            {
                return Err(PanelError::Unavailable);
            }
        }
        previous = n.id;
    }
    Ok(())
}
fn lock(file: &fs::File) -> PanelResult<()> {
    file.try_lock().map_err(unavailable)
}
impl Inbox {
    pub fn open(root: &Path) -> PanelResult<Self> {
        let marker = root.join("web-notifications.initialized.v1");
        let initialized = match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.file_type().is_file() && metadata.len() == 13 => {
                if fs::read(&marker).map_err(unavailable)? != b"{\"version\":1}" {
                    return Err(PanelError::Unavailable);
                }
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            _ => return Err(PanelError::Unavailable),
        };
        let directory = root.join("web-notifications");
        let fresh = match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_dir() => false,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !initialized => {
                fs::create_dir(&directory).map_err(unavailable)?;
                true
            }
            _ => return Err(PanelError::Unavailable),
        };
        let directory = directory.canonicalize().map_err(unavailable)?;
        let lock_path = directory.join("lock");
        if let Ok(metadata) = fs::symlink_metadata(&lock_path)
            && !metadata.file_type().is_file()
        {
            return Err(PanelError::Unavailable);
        }
        let guard = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(unavailable)?;
        lock(&guard)?;
        let path = directory.join("state.json");
        let ledger = if fresh {
            Ledger {
                version: 1,
                next_id: 1,
                seen: vec![],
                records: vec![],
            }
        } else {
            let metadata = fs::symlink_metadata(&path).map_err(unavailable)?;
            if !metadata.file_type().is_file() || metadata.len() > MAX_BYTES {
                return Err(PanelError::Unavailable);
            }
            let mut bytes = Vec::new();
            fs::File::open(path)
                .map_err(unavailable)?
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(unavailable)?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err(PanelError::Unavailable);
            }
            serde_json::from_slice(&bytes).map_err(unavailable)?
        };
        validate(&ledger)?;
        let inbox = Self {
            directory,
            _lock: DirectoryLock(guard),
            inner: Mutex::new(Inner {
                ledger,
                available: true,
                initialized: !fresh,
            }),
        };
        if fresh {
            let mut inner = inbox.inner.lock().map_err(unavailable)?;
            let ledger = inner.ledger.clone();
            inbox.commit(&mut inner, ledger)?;
        }
        // 目录已完整落盘但标记尚未写入时，可安全补写标记；现有坏/空目录仍拒绝重建。
        if !initialized {
            let mut temporary = tempfile::NamedTempFile::new_in(root).map_err(unavailable)?;
            temporary
                .write_all(b"{\"version\":1}")
                .map_err(unavailable)?;
            temporary.as_file().sync_all().map_err(unavailable)?;
            temporary.persist_noclobber(marker).map_err(unavailable)?;
            #[cfg(unix)]
            fs::File::open(root)
                .and_then(|f| f.sync_all())
                .map_err(unavailable)?;
        }
        Ok(inbox)
    }
    fn commit(&self, inner: &mut Inner, ledger: Ledger) -> PanelResult<()> {
        validate(&ledger)?;
        let bytes = serde_json::to_vec(&ledger).map_err(unavailable)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(PanelError::Capacity);
        }
        let save = || -> PanelResult<()> {
            if inner.initialized
                && !fs::symlink_metadata(self.directory.join("state.json"))
                    .map_err(unavailable)?
                    .file_type()
                    .is_file()
            {
                return Err(PanelError::Unavailable);
            }
            let mut temporary =
                tempfile::NamedTempFile::new_in(&self.directory).map_err(unavailable)?;
            temporary.write_all(&bytes).map_err(unavailable)?;
            temporary.as_file().sync_all().map_err(unavailable)?;
            temporary
                .persist(self.directory.join("state.json"))
                .map_err(unavailable)?;
            #[cfg(unix)]
            fs::File::open(&self.directory)
                .and_then(|f| f.sync_all())
                .map_err(unavailable)?;
            Ok(())
        };
        if let Err(error) = save() {
            inner.available = false;
            return Err(error);
        }
        inner.ledger = ledger;
        inner.initialized = true;
        Ok(())
    }
    fn insert(&self, inner: &mut Inner, mut record: Record) -> PanelResult<()> {
        if inner
            .ledger
            .seen
            .iter()
            .any(|s| s.source == record.source && s.events.contains(&record.event))
        {
            return Ok(());
        }
        let mut ledger = inner.ledger.clone();
        if ledger
            .seen
            .iter()
            .map(|seen| seen.events.len())
            .sum::<usize>()
            >= MAX_EVENTS
        {
            return Err(PanelError::Capacity);
        }
        if ledger.records.len() == MAX_ITEMS {
            let index = ledger
                .records
                .iter()
                .position(|r| r.notification.read_at_ms.is_some() && r.pending.is_none())
                .ok_or(PanelError::Capacity)?;
            ledger.records.remove(index);
        }
        record.notification.id = ledger.next_id;
        ledger.next_id = ledger
            .next_id
            .checked_add(1)
            .filter(|id| *id <= MAX_SAFE_ID)
            .ok_or(PanelError::Capacity)?;
        if let Some(seen) = ledger.seen.iter_mut().find(|s| s.source == record.source) {
            seen.events.push(record.event.clone());
        } else {
            ledger.seen.push(Seen {
                source: record.source.clone(),
                kind: if record.notification.kind == "trigger" {
                    "trigger"
                } else {
                    "task"
                }
                .into(),
                events: vec![record.event.clone()],
            });
        }
        ledger.records.push(record);
        if serde_json::to_vec(&ledger).map_err(unavailable)?.len() as u64
            > MAX_BYTES - RECEIPT_RESERVE_BYTES
        {
            return Err(PanelError::Capacity);
        }
        self.commit(inner, ledger)
    }
    fn settle(
        &self,
        inner: &mut Inner,
        tasks: Option<&dyn TaskAdmin>,
        triggers: Option<&dyn TriggerAdmin>,
        now_ms: u64,
    ) -> PanelResult<()> {
        let pending: Vec<_> = inner
            .ledger
            .records
            .iter()
            .filter(|r| r.pending.is_some())
            .cloned()
            .collect();
        for record in pending {
            let result = match record.pending.as_ref().expect("已筛选") {
                Settlement::Task {
                    owner,
                    id,
                    source,
                    created_at_ms,
                    finished_at_ms,
                } => {
                    let Some(tasks) = tasks else {
                        continue;
                    };
                    let task = tasks.get_owned(owner, id).map_err(unavailable)?;
                    if task
                        .as_ref()
                        .is_some_and(|t| t.id != *id || t.owner != *owner)
                    {
                        return Err(PanelError::Unavailable);
                    }
                    if task.as_ref().is_none_or(|t| {
                        t.source != *source
                            || t.created_at_ms != *created_at_ms
                            || t.finished_at_ms != Some(*finished_at_ms)
                    }) {
                        "superseded"
                    } else {
                        match tasks.record_local_delivery(
                            owner,
                            id,
                            *finished_at_ms,
                            &record.event,
                            now_ms.max(*finished_at_ms),
                        ) {
                            Ok(_) => "recorded",
                            Err(
                                eve_task_api::TaskError::InvalidTransition
                                | eve_task_api::TaskError::NotFound,
                            ) => {
                                // 执行器可能在两次短锁之间接续；再读真实批次，不把正常并发当坏存储。
                                let latest = tasks.get_owned(owner, id).map_err(unavailable)?;
                                if latest.is_none_or(|t| {
                                    t.source != *source
                                        || t.created_at_ms != *created_at_ms
                                        || t.finished_at_ms != Some(*finished_at_ms)
                                }) {
                                    "superseded"
                                } else {
                                    return Err(PanelError::Unavailable);
                                }
                            }
                            Err(_) => return Err(PanelError::Unavailable),
                        }
                    }
                }
                Settlement::Progress {
                    owner,
                    id,
                    source,
                    created_at_ms,
                    sequence,
                    at_ms,
                } => {
                    let Some(tasks) = tasks else {
                        continue;
                    };
                    let task = tasks.get_owned(owner, id).map_err(unavailable)?;
                    if task
                        .as_ref()
                        .is_some_and(|t| t.id != *id || t.owner != *owner)
                    {
                        return Err(PanelError::Unavailable);
                    }
                    if task
                        .as_ref()
                        .is_none_or(|t| t.source != *source || t.created_at_ms != *created_at_ms)
                    {
                        "superseded"
                    } else {
                        tasks
                            .record_local_progress(owner, id, *sequence, &record.event, *at_ms)
                            .map_err(unavailable)?;
                        "recorded"
                    }
                }
                Settlement::Trigger {
                    owner,
                    id,
                    source,
                    created_at_ms,
                    run,
                } => {
                    let Some(triggers) = triggers else {
                        continue;
                    };
                    let snapshot = triggers.snapshot().map_err(unavailable)?;
                    let trigger = snapshot.iter().find(|t| t.id == *id && t.owner == *owner);
                    if trigger.is_none_or(|t| {
                        t.source != *source
                            || t.created_at_ms != *created_at_ms
                            || t.runs != *run
                            || matches!(
                                t.status,
                                TriggerStatus::Armed
                                    | TriggerStatus::Ready
                                    | TriggerStatus::Running
                            )
                    }) {
                        "superseded"
                    } else {
                        match triggers.record_local_delivery(owner, id, *run) {
                            Ok(()) => "recorded",
                            Err(
                                eve_trigger_api::TriggerError::Transition
                                | eve_trigger_api::TriggerError::NotFound,
                            ) => {
                                let latest = triggers.snapshot().map_err(unavailable)?;
                                let latest =
                                    latest.iter().find(|t| t.id == *id && t.owner == *owner);
                                if latest.is_none_or(|t| {
                                    t.source != *source
                                        || t.created_at_ms != *created_at_ms
                                        || t.runs != *run
                                        || matches!(
                                            t.status,
                                            TriggerStatus::Armed
                                                | TriggerStatus::Ready
                                                | TriggerStatus::Running
                                        )
                                }) {
                                    "superseded"
                                } else {
                                    return Err(PanelError::Unavailable);
                                }
                            }
                            Err(_) => return Err(PanelError::Unavailable),
                        }
                    }
                }
            };
            let mut ledger = inner.ledger.clone();
            let saved = ledger
                .records
                .iter_mut()
                .find(|r| r.event == record.event)
                .ok_or(PanelError::Unavailable)?;
            saved.pending = None;
            saved.settlement = Some(result.into());
            self.commit(inner, ledger)?;
        }
        Ok(())
    }
    /// 仅可信宿主后台调用；任务和触发器都来自已认证的 Web owner 范围。
    pub fn collect(
        &self,
        tasks: &dyn TaskAdmin,
        triggers: &dyn TriggerAdmin,
        now_ms: u64,
    ) -> PanelResult<()> {
        self.collect_available(Some(tasks), Some(triggers), now_ms)
    }
    /// 未装配的能力不当作空账本；它的旧去重记录和待结算动作均保留。
    pub fn collect_available(
        &self,
        tasks: Option<&dyn TaskAdmin>,
        triggers: Option<&dyn TriggerAdmin>,
        now_ms: u64,
    ) -> PanelResult<()> {
        if now_ms == 0 {
            return Err(PanelError::InvalidInput);
        }
        let mut inner = self.inner.lock().map_err(unavailable)?;
        if !inner.available {
            return Err(PanelError::Unavailable);
        }
        self.settle(&mut inner, tasks, triggers, now_ms)?;
        let task_list = tasks
            .map(|tasks| tasks.scheduling_snapshot())
            .transpose()
            .map_err(unavailable)?;
        if let Some(task_list) = &task_list {
            task_list.validate().map_err(unavailable)?;
        }
        let trigger_list = triggers
            .map(|triggers| triggers.snapshot())
            .transpose()
            .map_err(unavailable)?
            .unwrap_or_default();
        if trigger_list.len() > eve_trigger_api::MAX_TRIGGERS {
            return Err(PanelError::Unavailable);
        }
        let mut live = BTreeSet::new();
        for task in task_list.iter().flat_map(|list| &list.tasks) {
            live.insert(source_key(
                &task.owner.session_id,
                &task.owner.user_id,
                &task.id,
                task.created_at_ms,
                "task",
            ));
        }
        let mut trigger_ids = BTreeSet::new();
        for trigger in &trigger_list {
            if !trigger_ids.insert(&trigger.id)
                || eve_task_api::validate_id(&trigger.id).is_err()
                || eve_task_api::validate_id(&trigger.source).is_err()
                || trigger.owner.validate().is_err()
                || !trigger.spec.valid()
                || trigger.created_at_ms == 0
                || trigger.result.as_ref().is_some_and(|s| s.len() > 32768)
                || trigger.runs > trigger.spec.recurrence.as_ref().map_or(1, |r| r.max_runs)
                || (trigger.status == TriggerStatus::Completed
                    && (trigger.result.as_ref().is_none_or(|s| s.trim().is_empty())
                        || trigger.delivery.is_none()))
            {
                return Err(PanelError::Unavailable);
            }
            live.insert(source_key(
                &trigger.owner.session_id,
                &trigger.owner.user_id,
                &trigger.id,
                trigger.created_at_ms,
                "trigger",
            ));
        }
        let retained = |seen: &Seen| {
            live.contains(&seen.source)
                || (seen.kind == "task" && tasks.is_none())
                || (seen.kind == "trigger" && triggers.is_none())
        };
        if inner.ledger.seen.iter().any(|s| !retained(s)) {
            let mut ledger = inner.ledger.clone();
            ledger.seen.retain(retained);
            self.commit(&mut inner, ledger)?;
        }
        for entry in task_list
            .iter()
            .flat_map(|list| &list.tasks)
            .filter(|t| t.status.finished())
        {
            let task_service = tasks.ok_or(PanelError::Unavailable)?;
            if !web_owner(&entry.owner.session_id, &entry.owner.user_id)
                || entry
                    .source
                    .starts_with(eve_task_api::IDLE_TASK_SOURCE_PREFIX)
            {
                continue;
            }
            let finished_at_ms = entry.finished_at_ms.ok_or(PanelError::Unavailable)?;
            let source = source_key(
                &entry.owner.session_id,
                &entry.owner.user_id,
                &entry.id,
                entry.created_at_ms,
                "task",
            );
            // 终态 epoch 由任务账本保证单调；已收录后无需每次复制可能很大的委派上下文。
            let event = hash(&(&source, finished_at_ms, entry.status));
            if inner
                .ledger
                .seen
                .iter()
                .any(|s| s.source == source && s.events.contains(&event))
            {
                continue;
            }
            let task = task_service
                .get_owned(&entry.owner, &entry.id)
                .map_err(unavailable)?
                .ok_or(PanelError::Unavailable)?;
            if !entry.matches(&task) {
                return Err(PanelError::Unavailable);
            }
            if task.status != entry.status || task.finished_at_ms != Some(finished_at_ms) {
                continue;
            }
            if finished_at_ms < task.created_at_ms
                || eve_task_api::validate_id(&task.spec.title).is_err()
                || task.spec.title.len() > eve_task_api::MAX_TITLE_BYTES
                || task
                    .result
                    .as_ref()
                    .is_some_and(|s| s.len() > eve_task_api::MAX_RESULT_BYTES)
                || (matches!(task.status, TaskStatus::Completed | TaskStatus::Blocked)
                    && task.result.as_ref().is_none_or(|s| s.trim().is_empty()))
            {
                return Err(PanelError::Unavailable);
            }
            let pending = task.notice_pending().then(|| Settlement::Task {
                owner: task.owner.clone(),
                id: task.id.clone(),
                source: task.source.clone(),
                created_at_ms: task.created_at_ms,
                finished_at_ms,
            });
            let (body, truncated) = crate::web_panel::clip(&task_body(&task), 8192);
            self.insert(
                &mut inner,
                Record {
                    source,
                    event,
                    pending,
                    settlement: None,
                    notification: Notification {
                        id: 0,
                        session_id: task.owner.session_id,
                        source_id: task.id,
                        kind: "task".into(),
                        status: task_status(task.status).into(),
                        title: task.spec.title,
                        body,
                        truncated,
                        created_at_ms: now_ms,
                        read_at_ms: None,
                    },
                },
            )?;
            self.settle(&mut inner, tasks, triggers, now_ms)?;
        }
        for trigger in trigger_list {
            if !web_owner(&trigger.owner.session_id, &trigger.owner.user_id) {
                continue;
            }
            let status = match trigger.status {
                TriggerStatus::Completed => "completed",
                TriggerStatus::Failed => "failed",
                TriggerStatus::Interrupted => "interrupted",
                TriggerStatus::Expired => "expired",
                TriggerStatus::Cancelled => "cancelled",
                _ => continue,
            };
            let source = source_key(
                &trigger.owner.session_id,
                &trigger.owner.user_id,
                &trigger.id,
                trigger.created_at_ms,
                "trigger",
            );
            let event = hash(&(&source, trigger.runs, status));
            let pending =
                (trigger.delivery == Some(eve_trigger_api::Delivery::Pending)).then(|| {
                    Settlement::Trigger {
                        owner: trigger.owner.clone(),
                        id: trigger.id.clone(),
                        source: trigger.source.clone(),
                        created_at_ms: trigger.created_at_ms,
                        run: trigger.runs,
                    }
                });
            let body = trigger.result.as_deref().unwrap_or(match trigger.status {
                TriggerStatus::Cancelled => "此提醒已取消。",
                TriggerStatus::Expired => "条件未在有效期内满足，提醒已到期。",
                _ => "触发器没有保存结果正文，请查看原会话记录核对。",
            });
            let (body, truncated) = crate::web_panel::clip(body, 8192);
            self.insert(
                &mut inner,
                Record {
                    source,
                    event,
                    pending,
                    settlement: None,
                    notification: Notification {
                        id: 0,
                        session_id: trigger.owner.session_id,
                        source_id: trigger.id,
                        kind: "trigger".into(),
                        status: status.into(),
                        title: trigger.spec.title,
                        body,
                        truncated,
                        created_at_ms: now_ms,
                        read_at_ms: None,
                    },
                },
            )?;
            self.settle(&mut inner, tasks, triggers, now_ms)?;
        }
        let Some(task_service) = tasks else {
            return Ok(());
        };
        let progress = task_service.progress_snapshot().map_err(unavailable)?;
        progress.validate().map_err(unavailable)?;
        let mut scopes = BTreeSet::new();
        for entry in progress.due(now_ms) {
            if !web_owner(&entry.owner.session_id, &entry.owner.user_id)
                || !scopes.insert((&entry.owner.session_id, &entry.owner.user_id))
            {
                continue;
            }
            let task = task_service
                .get_owned(&entry.owner, &entry.id)
                .map_err(unavailable)?
                .ok_or(PanelError::Unavailable)?;
            if !entry.matches(&task) {
                return Err(PanelError::Unavailable);
            }
            let latest = eve_task_api::TaskProgressEntry::from(&task);
            if !latest.due(now_ms) {
                continue;
            }
            let sequence = latest.next_sequence().ok_or(PanelError::Unavailable)?;
            let source = source_key(
                &task.owner.session_id,
                &task.owner.user_id,
                &task.id,
                task.created_at_ms,
                "task",
            );
            let event = hash(&(&source, "progress", sequence));
            let (body, truncated) = crate::web_panel::clip(&progress_body(&task, now_ms), 8192);
            let pending = Some(Settlement::Progress {
                owner: task.owner.clone(),
                id: task.id.clone(),
                source: task.source.clone(),
                created_at_ms: task.created_at_ms,
                sequence,
                at_ms: now_ms,
            });
            self.insert(
                &mut inner,
                Record {
                    source,
                    event,
                    pending,
                    settlement: None,
                    notification: Notification {
                        id: 0,
                        session_id: task.owner.session_id,
                        source_id: task.id,
                        kind: "progress".into(),
                        status: task_status(task.status).into(),
                        title: task.spec.title,
                        body,
                        truncated,
                        created_at_ms: now_ms,
                        read_at_ms: None,
                    },
                },
            )?;
            self.settle(&mut inner, tasks, triggers, now_ms)?;
        }
        Ok(())
    }
}
fn task_status(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Interrupted => "interrupted",
    }
}
fn task_body(task: &Task) -> String {
    match task.status {
        TaskStatus::Completed => task
            .result
            .clone()
            .unwrap_or_else(|| "执行器标记完成，但未保存结果正文；需要核对。".into()),
        TaskStatus::Blocked => format!(
            "任务尚未完成，已有步骤已保留。\n{}",
            task.result.as_deref().unwrap_or("等待补充信息或解除阻塞。")
        ),
        TaskStatus::Failed => format!(
            "任务未完成：{}",
            task.failure.as_deref().unwrap_or("原因未记录，需要核对。")
        ),
        TaskStatus::Cancelled => "任务已取消。".into(),
        TaskStatus::Interrupted => format!(
            "执行已中断，不能据此判断目标完成。\n{}",
            task.failure
                .as_deref()
                .unwrap_or("已有记录保留，需要核对。")
        ),
        _ => "任务仍在执行。".into(),
    }
}
fn progress_body(task: &Task, now_ms: u64) -> String {
    let execution = task.execution.as_ref();
    let label = match task.status {
        TaskStatus::Queued => "仍在排队，尚未开始",
        TaskStatus::Blocked => "本次执行已停止，等待补充信息或解除阻塞",
        TaskStatus::Running if task.cancel_requested => "已收到停止请求，等待执行器确认停止",
        TaskStatus::Running
            if execution.is_some_and(|e| {
                e.authorization_deadline_at_ms
                    .is_some_and(|at| at <= now_ms)
                    || e.deadline_at_ms.is_some_and(|at| at <= now_ms)
            }) =>
        {
            "原授权或执行期限已到，等待停止状态核对；没有新的执行确认"
        }
        TaskStatus::Running
            if execution
                .and_then(|e| e.resume_at_ms)
                .is_some_and(|at| at > now_ms) =>
        {
            "等待自动恢复，恢复时点尚未到达"
        }
        TaskStatus::Running if execution.and_then(|e| e.resume_at_ms).is_some() => {
            "已到自动恢复时点，等待调度接续；尚无新的执行确认"
        }
        TaskStatus::Running => "执行器已接收任务；当前进程是否仍在执行尚无独立确认",
        _ => "已结束本次执行，请以结局通知为准",
    };
    let checkpoint = execution.and_then(|e| e.checkpoint.as_ref());
    let remaining = checkpoint.and_then(|c| c.remaining.as_deref());
    let mut body = format!("30 分钟进度提醒：任务{label}，尚未确认完成。");
    if let Some(remaining) = remaining {
        body.push_str(&format!("\n已保存的剩余工作：{remaining}"));
    }
    if task.status == TaskStatus::Blocked
        && let Some(result) = &task.result
    {
        body.push_str(&format!("\n阻塞记录：{result}"));
    }
    if remaining.is_none() && checkpoint.is_none() {
        body.push_str("\n尚无可报告的新检查点；本条只确认账本中的状态。");
    }
    body
}
impl WebInbox for Inbox {
    fn list(&self, before: Option<u64>, limit: usize) -> PanelResult<NotificationPage> {
        if !(1..=100).contains(&limit) || before == Some(0) {
            return Err(PanelError::InvalidInput);
        }
        let inner = self.inner.lock().map_err(unavailable)?;
        if !inner.available {
            return Err(PanelError::Unavailable);
        }
        let unread = inner
            .ledger
            .records
            .iter()
            .filter(|r| r.notification.read_at_ms.is_none())
            .count();
        let mut items: Vec<_> = inner
            .ledger
            .records
            .iter()
            .rev()
            .filter(|r| before.is_none_or(|id| r.notification.id < id))
            .take(limit + 1)
            .map(|r| r.notification.clone())
            .collect();
        let next_before = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|n| n.id)
        } else {
            None
        };
        Ok(NotificationPage {
            items,
            next_before,
            unread,
        })
    }
    fn mark_read(&self, id: u64, now_ms: u64) -> PanelResult<bool> {
        if id == 0 || now_ms == 0 {
            return Err(PanelError::InvalidInput);
        }
        let mut inner = self.inner.lock().map_err(unavailable)?;
        if !inner.available {
            return Err(PanelError::Unavailable);
        }
        let mut ledger = inner.ledger.clone();
        let record = ledger
            .records
            .iter_mut()
            .find(|r| r.notification.id == id)
            .ok_or(PanelError::NotFound)?;
        if record.notification.read_at_ms.is_some() {
            return Ok(false);
        }
        record.notification.read_at_ms = Some(now_ms.max(record.notification.created_at_ms));
        self.commit(&mut inner, ledger)?;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "web_notifications_tests.rs"]
mod tests;
