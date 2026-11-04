//! 认知与自身兴趣的持久历史。上下文有界，历史及来源去重不会因视图容量被删除。
use eve_subject_api::*;
use ring::digest::{SHA256, digest};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub struct SqliteIdleMemory {
    database: Mutex<Connection>,
    healthy: AtomicBool,
    _exclusive: File,
}

fn storage(_: rusqlite::Error) -> SubjectError {
    SubjectError::Storage
}
fn corrupt(_: impl std::fmt::Display) -> SubjectError {
    SubjectError::CorruptState
}
fn encode<T: Serialize>(value: &T) -> SubjectResult<String> {
    serde_json::to_string(value).map_err(|_| SubjectError::InvalidInput)
}
fn decode<T: DeserializeOwned>(value: &str) -> SubjectResult<T> {
    if value.len() > 256 * 1024 {
        return Err(SubjectError::CorruptState);
    }
    serde_json::from_str(value).map_err(corrupt)
}
fn scope_key(scope: &SubjectScope) -> SubjectResult<String> {
    validate_scope(scope)?;
    encode(scope)
}
fn bounded_integer(value: u64) -> SubjectResult<i64> {
    i64::try_from(value).map_err(|_| SubjectError::LimitReached)
}
fn fingerprint(observation: &IdleObservation, result: &IdleReflection) -> SubjectResult<String> {
    let text = encode(&(observation, result))?;
    if text.len() > 256 * 1024 {
        return Err(SubjectError::LimitReached);
    }
    Ok(digest(&SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn valid_record(payload: &str) -> SubjectResult<SelfInterestRecord> {
    let record: SelfInterestRecord = decode(payload)?;
    validate_self_interests(std::slice::from_ref(&record)).map_err(corrupt)?;
    Ok(record)
}
struct InterestRow {
    key: String,
    revision: u64,
    payload: String,
    topic: String,
    active: bool,
    strength: u8,
    updated_at: u64,
}
fn current_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InterestRow> {
    Ok(InterestRow {
        key: row.get(0)?,
        revision: row.get(1)?,
        payload: row.get(2)?,
        topic: row.get(3)?,
        active: row.get(4)?,
        strength: row.get(5)?,
        updated_at: row.get(6)?,
    })
}
fn checked_record(row: InterestRow) -> SubjectResult<SelfInterestRecord> {
    let record = valid_record(&row.payload)?;
    if record.current.key != row.key
        || record.revision != row.revision
        || self_interest_topic_key(&record.current.topic) != row.topic
        || record.current.phase.motivates_action() != row.active
        || record.current.strength != row.strength
        || record.updated_at_ms != row.updated_at
    {
        return Err(SubjectError::CorruptState);
    }
    Ok(record)
}
fn selected(db: &Connection, scope: &str) -> SubjectResult<Vec<SelfInterestRecord>> {
    let mut stmt = db.prepare("SELECT key,revision,payload,topic_key,active,strength,updated_at FROM interests WHERE scope=?1 ORDER BY active DESC,strength DESC,updated_at DESC,key LIMIT ?2").map_err(storage)?;
    let rows = stmt
        .query_map(params![scope, MAX_SELF_INTERESTS as u64], current_row)
        .map_err(storage)?;
    let mut selected = vec![];
    for row in rows {
        let record = checked_record(row.map_err(storage)?)?;
        selected.push(record);
        if validate_self_interests(&selected) == Err(SubjectError::LimitReached) {
            selected.pop();
        } else {
            validate_self_interests(&selected).map_err(corrupt)?;
        }
    }
    Ok(selected)
}
fn snapshot(db: &Connection, scope: &str) -> SubjectResult<IdleMind> {
    let stored: Option<(u64, String)> = db
        .query_row(
            "SELECT revision,payload FROM minds WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage)?;
    let Some((revision, payload)) = stored else {
        let orphan: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM interests WHERE scope=?1 UNION ALL SELECT 1 FROM interest_history WHERE scope=?1 UNION ALL SELECT 1 FROM reflections WHERE scope=?1 UNION ALL SELECT 1 FROM consumed WHERE scope=?1)", [scope], |r| r.get(0)).map_err(storage)?;
        if orphan {
            return Err(SubjectError::CorruptState);
        }
        return Ok(IdleMind::default());
    };
    let mut mind: IdleMind = decode(&payload)?;
    if revision == 0
        || mind.revision != revision
        || !mind.archived
        || !mind.self_interests.is_empty()
        || mind.last_task.is_empty()
        || mind.next_at_ms == 0
        || validate_id(&mind.last_task).is_err()
        || mind.thought.trim().is_empty()
        || mind.reason.trim().is_empty()
        || mind.thought.len() > 1024
        || mind.reason.len() > 768
    {
        return Err(SubjectError::CorruptState);
    }
    validate_judgments(&mind.judgments).map_err(corrupt)?;
    mind.self_interests = selected(db, scope)?;
    // 整份当前认知也受关注快照上限约束，未展示兴趣继续留在独立历史中。
    while encode(&mind)?.len() > MAX_PAYLOAD_BYTES {
        if mind.self_interests.pop().is_none() {
            return Err(SubjectError::CorruptState);
        }
    }
    Ok(mind)
}
fn existing(db: &Connection, scope: &str, key: &str) -> SubjectResult<Option<SelfInterestRecord>> {
    db.query_row(
        "SELECT key,revision,payload,topic_key,active,strength,updated_at FROM interests WHERE scope=?1 AND key=?2",
        params![scope, key],
        current_row,
    )
    .optional()
    .map_err(storage)?
    .map(checked_record)
    .transpose()
}
fn consumed(db: &Connection, scope: &str, key: &str, evidence: &str) -> SubjectResult<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM consumed WHERE scope=?1 AND key=?2 AND evidence_id=?3)",
        params![scope, key, evidence],
        |r| r.get(0),
    )
    .map_err(storage)
}
fn prepare(db: &Connection, observation: &mut IdleObservation) -> SubjectResult<()> {
    observation.validate()?;
    let scope = scope_key(&observation.scope)?;
    let actual_mind = snapshot(db, &scope)?;
    if actual_mind.revision != observation.revision
        || actual_mind.judgments != observation.judgments
    {
        return Err(SubjectError::InvalidTransition);
    }
    observation.interest_consumed.clear();
    for record in &observation.self_interests {
        let actual =
            existing(db, &scope, &record.current.key)?.ok_or(SubjectError::InvalidTransition)?;
        if &actual != record {
            return Err(SubjectError::InvalidTransition);
        }
        let mut used = vec![];
        for evidence in &observation.evidence {
            if consumed(db, &scope, &record.current.key, &evidence.id)? {
                used.push(evidence.id.clone());
            }
        }
        if !used.is_empty() {
            observation
                .interest_consumed
                .insert(record.current.key.clone(), used);
        }
    }
    // 去重标记仅覆盖当前材料，不随历史增长；兴趣正文仍有独立上下文上限。
    observation.validate()
}
fn save_interest(
    tx: &Transaction<'_>,
    scope: &str,
    record: &SelfInterestRecord,
    task: &str,
) -> SubjectResult<()> {
    validate_self_interests(std::slice::from_ref(record))?;
    let payload = encode(record)?;
    tx.execute("INSERT INTO interests(scope,key,topic_key,revision,payload,active,strength,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(scope,key) DO UPDATE SET revision=excluded.revision,payload=excluded.payload,active=excluded.active,strength=excluded.strength,updated_at=excluded.updated_at",
        params![scope,record.current.key,self_interest_topic_key(&record.current.topic),bounded_integer(record.revision)?,payload,record.current.phase.motivates_action(),record.current.strength,bounded_integer(record.updated_at_ms)?]).map_err(storage)?;
    tx.execute(
        "INSERT INTO interest_history(scope,key,revision,payload,task_id) VALUES(?1,?2,?3,?4,?5)",
        params![
            scope,
            record.current.key,
            bounded_integer(record.revision)?,
            payload,
            task
        ],
    )
    .map_err(storage)?;
    for evidence in &record.consumed_evidence {
        tx.execute(
            "INSERT OR IGNORE INTO consumed(scope,key,evidence_id) VALUES(?1,?2,?3)",
            params![scope, record.current.key, evidence],
        )
        .map_err(storage)?;
    }
    Ok(())
}

impl SqliteIdleMemory {
    pub fn open(path: &Path) -> SubjectResult<Self> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(SubjectError::CorruptState);
        }
        let exists = path.exists();
        let marker = path.with_extension("required");
        let marker_exists = marker.symlink_metadata().is_ok();
        if marker_exists
            && (!marker
                .symlink_metadata()
                .map_err(corrupt)?
                .file_type()
                .is_file()
                || std::fs::read(&marker).map_err(corrupt)? != b"self-interests-v1\n"
                || !exists)
        {
            return Err(SubjectError::CorruptState);
        }
        let lock = path.with_extension("lock");
        if lock
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(SubjectError::CorruptState);
        }
        let exclusive = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock)
            .map_err(|_| SubjectError::Storage)?;
        exclusive
            .try_lock()
            .map_err(|_| SubjectError::Unavailable)?;
        let db = Connection::open(path).map_err(corrupt)?;
        db.busy_timeout(Duration::from_millis(500))
            .map_err(storage)?;
        if !exists {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| SubjectError::Storage)?;
            }
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE metadata(version INTEGER NOT NULL); INSERT INTO metadata VALUES(1);
                CREATE TABLE minds(scope TEXT PRIMARY KEY,revision INTEGER NOT NULL,payload TEXT NOT NULL);
                CREATE TABLE reflections(scope TEXT NOT NULL,revision INTEGER NOT NULL,task_id TEXT NOT NULL,fingerprint TEXT NOT NULL,observation TEXT NOT NULL,result TEXT NOT NULL,at_ms INTEGER NOT NULL,PRIMARY KEY(scope,revision),UNIQUE(scope,task_id));
                CREATE TABLE interests(scope TEXT NOT NULL,key TEXT NOT NULL,topic_key TEXT NOT NULL,revision INTEGER NOT NULL,payload TEXT NOT NULL,active INTEGER NOT NULL,strength INTEGER NOT NULL,updated_at INTEGER NOT NULL,PRIMARY KEY(scope,key),UNIQUE(scope,topic_key));
                CREATE INDEX interest_selection ON interests(scope,active DESC,strength DESC,updated_at DESC,key);
                CREATE TABLE interest_history(scope TEXT NOT NULL,key TEXT NOT NULL,revision INTEGER NOT NULL,payload TEXT NOT NULL,task_id TEXT NOT NULL,PRIMARY KEY(scope,key,revision));
                CREATE TABLE consumed(scope TEXT NOT NULL,key TEXT NOT NULL,evidence_id TEXT NOT NULL,PRIMARY KEY(scope,key,evidence_id));
                COMMIT;").map_err(storage)?;
        }
        let versions: Vec<u32> = db
            .prepare("SELECT version FROM metadata")
            .map_err(corrupt)?
            .query_map([], |r| r.get(0))
            .map_err(corrupt)?
            .collect::<Result<_, _>>()
            .map_err(corrupt)?;
        if versions != [1] {
            return Err(SubjectError::UnsupportedVersion);
        }
        let check: String = db
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(corrupt)?;
        if check != "ok" {
            return Err(SubjectError::CorruptState);
        }
        for query in [
            "SELECT scope,revision,payload FROM minds LIMIT 0",
            "SELECT scope,revision,task_id,fingerprint,observation,result,at_ms FROM reflections LIMIT 0",
            "SELECT scope,key,topic_key,revision,payload,active,strength,updated_at FROM interests LIMIT 0",
            "SELECT scope,key,revision,payload,task_id FROM interest_history LIMIT 0",
            "SELECT scope,key,evidence_id FROM consumed LIMIT 0",
        ] {
            db.prepare(query).map_err(corrupt)?;
        }
        db.pragma_update(None, "journal_mode", "DELETE")
            .map_err(storage)?;
        db.pragma_update(None, "synchronous", "FULL")
            .map_err(storage)?;
        if !marker_exists {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&marker)
                .map_err(|_| SubjectError::Storage)?;
            file.write_all(b"self-interests-v1\n")
                .and_then(|()| file.sync_all())
                .map_err(|_| SubjectError::Storage)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                File::open(parent)
                    .and_then(|f| f.sync_all())
                    .map_err(|_| SubjectError::Storage)?;
            }
        }
        Ok(Self {
            database: Mutex::new(db),
            healthy: AtomicBool::new(true),
            _exclusive: exclusive,
        })
    }
    fn run<T>(&self, f: impl FnOnce(&mut Connection) -> SubjectResult<T>) -> SubjectResult<T> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(SubjectError::Unavailable);
        }
        let mut db = self
            .database
            .lock()
            .map_err(|_| SubjectError::Unavailable)?;
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(SubjectError::Unavailable);
        }
        let result = f(&mut db);
        if matches!(
            result,
            Err(SubjectError::Storage | SubjectError::CorruptState)
        ) {
            self.healthy.store(false, Ordering::SeqCst);
        }
        result
    }
}

impl IdleMemoryStore for SqliteIdleMemory {
    fn snapshot(&self, scope: &SubjectScope) -> SubjectResult<IdleMind> {
        let key = scope_key(scope)?;
        self.run(|db| snapshot(db, &key))
    }
    fn prepare_observation(&self, observation: &mut IdleObservation) -> SubjectResult<()> {
        self.run(|db| prepare(db, observation))
    }
    fn commit(
        &self,
        observation: &IdleObservation,
        result: &IdleReflection,
        task: &str,
        now: u64,
    ) -> SubjectResult<IdleMind> {
        observation.validate()?;
        result.validate()?;
        validate_id(task)?;
        if now == 0 {
            return Err(SubjectError::InvalidInput);
        }
        bounded_integer(now)?;
        let key = scope_key(&observation.scope)?;
        let hash = fingerprint(observation, result)?;
        self.run(|db|{
            let tx=db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
            let prior_hash:Option<String>=tx.query_row("SELECT fingerprint FROM reflections WHERE scope=?1 AND task_id=?2",params![key,task],|r|r.get(0)).optional().map_err(storage)?;
            if let Some(prior_hash)=prior_hash {
                if prior_hash!=hash {return Err(SubjectError::InvalidTransition);}
                return snapshot(&tx,&key);
            }
            let mut previous=snapshot(&tx,&key)?;
            if observation.revision!=previous.revision {return Err(SubjectError::InvalidTransition);}
            let mut checked=observation.clone();
            // 精确兴趣可来自历史分页读取，而不必挤进本次自动上下文。
            for update in &result.interest_updates {
                if !checked.self_interests.iter().any(|r|r.current.key==update.key)
                    && let Some(record)=existing(&tx,&key,&update.key)? {checked.self_interests.push(record);}
                let topic_owner:Option<String>=tx.query_row("SELECT key FROM interests WHERE scope=?1 AND topic_key=?2",params![key,self_interest_topic_key(&update.topic)],|r|r.get(0)).optional().map_err(storage)?;
                if topic_owner.is_some_and(|owner|owner!=update.key) {return Err(SubjectError::InvalidInput);}
            }
            // 输出最多两个更新；保留相关项核对，其他项不参与当前修订。
            let needed: BTreeSet<_>=result.interest_updates.iter().map(|u|u.key.as_str()).chain(result.action.iter().filter_map(|a|a.interest_key.as_deref())).collect();
            checked.self_interests.retain(|r|needed.contains(r.current.key.as_str()));
            checked.interest_consumed.clear();
            if let Some(action_key)=result.action.as_ref().and_then(|a|a.interest_key.as_ref())
                && !checked.self_interests.iter().any(|r|&r.current.key==action_key)
                && let Some(record)=existing(&tx,&key,action_key)? {checked.self_interests.push(record);}
            prepare(&tx,&mut checked)?;
            eve_subject_plugin::validate_idle_reflection(&checked,result)?;
            for update in &result.interest_updates {
                let old=existing(&tx,&key,&update.key)?;
                if old.as_ref().is_some_and(|old|old.current==*update) {continue;}
                let revision=old.as_ref().map_or(Ok(1),|r|r.revision.checked_add(1).ok_or(SubjectError::LimitReached))?;
                if old.as_ref().is_some_and(|r|now<r.updated_at_ms) {return Err(SubjectError::InvalidInput);}
                let record=SelfInterestRecord{current:update.clone(),revision,first_recorded_at_ms:old.as_ref().map_or(now,|r|r.first_recorded_at_ms),updated_at_ms:now,consumed_evidence:update.evidence.clone()};
                save_interest(&tx,&key,&record,task)?;
            }
            previous.self_interests.clear();
            let mut thoughts=result.clone(); thoughts.interest_updates.clear();
            let mut next=eve_subject_plugin::revise_idle_mind(previous,&thoughts,task,now)?;
            next.archived=true;
            tx.execute("INSERT INTO minds(scope,revision,payload) VALUES(?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision,payload=excluded.payload",params![key,bounded_integer(next.revision)?,encode(&next)?]).map_err(storage)?;
            tx.execute("INSERT INTO reflections(scope,revision,task_id,fingerprint,observation,result,at_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![key,bounded_integer(next.revision)?,task,hash,encode(observation)?,encode(result)?,bounded_integer(now)?]).map_err(storage)?;
            next=snapshot(&tx,&key)?;
            tx.commit().map_err(storage)?;
            Ok(next)
        })
    }
    fn interests(
        &self,
        scope: &SubjectScope,
        after: Option<&str>,
        limit: usize,
    ) -> SubjectResult<SelfInterestPage> {
        if limit == 0 || limit > MAX_SELF_INTEREST_PAGE {
            return Err(SubjectError::InvalidInput);
        }
        if let Some(after) = after {
            validate_id(after)?;
        }
        let key = scope_key(scope)?;
        self.run(|db| {
            let tx = db.transaction().map_err(storage)?;
            let revision = snapshot(&tx, &key)?.revision;
            let total = tx
                .query_row(
                    "SELECT count(*) FROM interests WHERE scope=?1",
                    [&key],
                    |r| r.get(0),
                )
                .map_err(storage)?;
            let mut stmt = tx
                .prepare(
                    "SELECT key,revision,payload,topic_key,active,strength,updated_at FROM interests WHERE scope=?1 AND key>?2 ORDER BY key LIMIT ?3",
                )
                .map_err(storage)?;
            let mut items = stmt
                .query_map(params![key, after.unwrap_or(""), limit as u64 + 1], current_row)
                .map_err(storage)?
                .map(|r| checked_record(r.map_err(storage)?))
                .collect::<SubjectResult<Vec<_>>>()?;
            let next_after = if items.len() > limit {
                items.pop();
                items.last().map(|r| r.current.key.clone())
            } else {
                None
            };
            Ok(SelfInterestPage {
                revision,
                total,
                items,
                next_after,
            })
        })
    }
    fn history(
        &self,
        scope: &SubjectScope,
        interest: &str,
        after: u64,
        limit: usize,
    ) -> SubjectResult<SelfInterestHistoryPage> {
        if limit == 0 || limit > MAX_SELF_INTEREST_PAGE {
            return Err(SubjectError::InvalidInput);
        }
        validate_id(interest)?;
        bounded_integer(after)?;
        let key = scope_key(scope)?;
        self.run(|db|{
            let mut stmt=db.prepare("SELECT revision,payload FROM interest_history WHERE scope=?1 AND key=?2 AND revision>?3 ORDER BY revision LIMIT ?4").map_err(storage)?;
            let mut items=stmt.query_map(params![key,interest,after,limit as u64+1],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?))).map_err(storage)?.map(|r|{
                let (revision,payload)=r.map_err(storage)?;
                let record=valid_record(&payload)?;
                if record.revision!=revision || record.current.key!=interest {return Err(SubjectError::CorruptState);}
                Ok(record)
            }).collect::<SubjectResult<Vec<_>>>()?;
            let next_after=if items.len()>limit {items.pop();items.last().map(|r|r.revision)}else{None};
            Ok(SelfInterestHistoryPage{items,next_after})
        })
    }
    fn import_legacy(&self, scope: &SubjectScope, mind: &IdleMind) -> SubjectResult<()> {
        let key = scope_key(scope)?;
        validate_self_interests(&mind.self_interests)?;
        validate_judgments(&mind.judgments)?;
        if mind.revision == 0 {
            return Ok(());
        }
        if mind.archived
            || mind.next_at_ms == 0
            || validate_id(&mind.last_task).is_err()
            || mind.thought.trim().is_empty()
            || mind.thought.len() > 1024
            || mind.reason.trim().is_empty()
            || mind.reason.len() > 768
            || encode(mind)?.len() > MAX_PAYLOAD_BYTES
        {
            return Err(SubjectError::InvalidInput);
        }
        self.run(|db| {
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(storage)?;
            if snapshot(&tx, &key)?.revision != 0 {
                return Err(SubjectError::InvalidTransition);
            }
            for record in &mind.self_interests {
                save_interest(&tx, &key, record, &mind.last_task)?;
            }
            let mut value: IdleMind = decode(&encode(mind)?)?;
            value.self_interests.clear();
            value.archived = true;
            tx.execute(
                "INSERT INTO minds(scope,revision,payload) VALUES(?1,?2,?3)",
                params![key, bounded_integer(mind.revision)?, encode(&value)?],
            )
            .map_err(storage)?;
            tx.commit().map_err(storage)
        })
    }
}
