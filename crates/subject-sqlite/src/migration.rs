use super::*;

const AUTONOMY_SCHEMA: &str = "
    CREATE TABLE host_authorizations(scope TEXT PRIMARY KEY,authorization_id TEXT NOT NULL,digest TEXT NOT NULL,at_ms INTEGER NOT NULL);
    CREATE TABLE autonomy_problems(scope TEXT NOT NULL,problem TEXT NOT NULL,focus TEXT NOT NULL,at_ms INTEGER NOT NULL,PRIMARY KEY(scope,problem));
    CREATE TABLE reconciled_tasks(scope TEXT NOT NULL,task TEXT NOT NULL,outcome TEXT NOT NULL,at_ms INTEGER NOT NULL,PRIMARY KEY(scope,task,outcome));
";

const EXPLORATION_SCHEMA: &str = "
    CREATE TABLE exploration_experiences(seq INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL UNIQUE,scope TEXT NOT NULL,started_at INTEGER NOT NULL,ended_at INTEGER NOT NULL,payload TEXT NOT NULL,digest TEXT NOT NULL);
    CREATE INDEX exploration_recent ON exploration_experiences(scope,seq DESC);
    CREATE TABLE exploration_sources(scope TEXT NOT NULL,reflection_id TEXT NOT NULL,focus_id TEXT NOT NULL,at_ms INTEGER NOT NULL,PRIMARY KEY(scope,reflection_id),UNIQUE(scope,focus_id));
";

pub(super) fn prepare(db: &mut Connection, version: u32) -> SubjectResult<()> {
    if version >= 2 {
        for query in [
            "SELECT scope,authorization_id,digest,at_ms FROM host_authorizations LIMIT 0",
            "SELECT scope,problem,focus,at_ms FROM autonomy_problems LIMIT 0",
            "SELECT scope,task,outcome,at_ms FROM reconciled_tasks LIMIT 0",
        ] {
            db.prepare(query).map_err(corrupt)?;
        }
    }
    if version == 3 {
        for query in [
            "SELECT seq,session_id,scope,started_at,ended_at,payload,digest FROM exploration_experiences LIMIT 0",
            "SELECT scope,reflection_id,focus_id,at_ms FROM exploration_sources LIMIT 0",
        ] {
            db.prepare(query).map_err(corrupt)?;
        }
    } else {
        validate_legacy(db, version)?;
    }
    db.pragma_update(None, "journal_mode", "DELETE")
        .map_err(storage)?;
    db.pragma_update(None, "synchronous", "FULL")
        .map_err(storage)?;
    if version == 3 {
        return Ok(());
    }
    // 一个事务跨越全部所需版本；中途失败时连版本号也保持原样。
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(storage)?;
    if version == 1 {
        tx.execute_batch(AUTONOMY_SCHEMA).map_err(storage)?;
    }
    tx.execute_batch(EXPLORATION_SCHEMA).map_err(storage)?;
    tx.execute("UPDATE metadata SET version=3", [])
        .map_err(storage)?;
    tx.commit().map_err(storage)
}

fn checked_scope(key: &str) -> SubjectResult<()> {
    let scope: SubjectScope = decode(key)?;
    if scope_key(&scope).map_err(corrupt)? != key {
        return Err(SubjectError::CorruptState);
    }
    Ok(())
}

fn validate_legacy(db: &Connection, version: u32) -> SubjectResult<()> {
    let mut scopes = db.prepare("SELECT scope FROM minds").map_err(corrupt)?;
    for key in scopes
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(corrupt)?
    {
        let key = key.map_err(corrupt)?;
        checked_scope(&key)?;
        snapshot(db, &key).map_err(corrupt)?;
    }
    let orphan: bool = db
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM (SELECT scope FROM interests UNION SELECT scope FROM interest_history UNION SELECT scope FROM reflections UNION SELECT scope FROM consumed) data LEFT JOIN minds ON minds.scope=data.scope WHERE minds.scope IS NULL)",
            [],
            |r| r.get(0),
        )
        .map_err(corrupt)?;
    if orphan {
        return Err(SubjectError::CorruptState);
    }
    validate_interests(db)?;
    validate_reflections(db)?;
    if version >= 2 {
        validate_autonomy(db)?;
    }
    Ok(())
}

fn validate_interests(db: &Connection) -> SubjectResult<()> {
    let mut interests = db
        .prepare(
            "SELECT key,revision,payload,topic_key,active,strength,updated_at,scope FROM interests",
        )
        .map_err(corrupt)?;
    let records = interests
        .query_map([], |row| Ok((current_row(row)?, row.get::<_, String>(7)?)))
        .map_err(corrupt)?;
    for record in records {
        let (row, scope) = record.map_err(corrupt)?;
        checked_scope(&scope)?;
        let record = checked_record(row)?;
        let historical: Option<(u64, String)> = db
            .query_row(
                "SELECT revision,payload FROM interest_history WHERE scope=?1 AND key=?2 ORDER BY revision DESC LIMIT 1",
                params![scope, record.current.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(corrupt)?;
        let Some((revision, payload)) = historical else {
            return Err(SubjectError::CorruptState);
        };
        if revision != record.revision || valid_record(&payload)? != record {
            return Err(SubjectError::CorruptState);
        }
    }
    let mut history = db
        .prepare("SELECT scope,key,revision,payload,task_id FROM interest_history ORDER BY scope,key,revision")
        .map_err(corrupt)?;
    let mut rows = history.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let scope: String = row.get(0).map_err(corrupt)?;
        let key: String = row.get(1).map_err(corrupt)?;
        let revision: u64 = row.get(2).map_err(corrupt)?;
        let payload: String = row.get(3).map_err(corrupt)?;
        let task: String = row.get(4).map_err(corrupt)?;
        let record = valid_record(&payload)?;
        checked_scope(&scope)?;
        validate_id(&task).map_err(corrupt)?;
        let current = existing(db, &scope, &key)?.ok_or(SubjectError::CorruptState)?;
        if record.current.key != key
            || record.revision != revision
            || record.revision > current.revision
            || record.first_recorded_at_ms != current.first_recorded_at_ms
            || record.updated_at_ms > current.updated_at_ms
            || self_interest_topic_key(&record.current.topic)
                != self_interest_topic_key(&current.current.topic)
        {
            return Err(SubjectError::CorruptState);
        }
        for id in &record.consumed_evidence {
            if !consumed(db, &scope, &key, id).map_err(corrupt)? {
                return Err(SubjectError::CorruptState);
            }
        }
    }
    let mut consumed = db
        .prepare("SELECT scope,key,evidence_id FROM consumed")
        .map_err(corrupt)?;
    let mut rows = consumed.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let scope: String = row.get(0).map_err(corrupt)?;
        let key: String = row.get(1).map_err(corrupt)?;
        let evidence: String = row.get(2).map_err(corrupt)?;
        checked_scope(&scope)?;
        validate_id(&key).map_err(corrupt)?;
        validate_id(&evidence).map_err(corrupt)?;
        if existing(db, &scope, &key)?.is_none() {
            return Err(SubjectError::CorruptState);
        }
    }
    Ok(())
}

fn validate_reflections(db: &Connection) -> SubjectResult<()> {
    let mut reflections = db
        .prepare(
            "SELECT scope,revision,task_id,fingerprint,observation,result,at_ms FROM reflections",
        )
        .map_err(corrupt)?;
    let mut rows = reflections.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let scope: String = row.get(0).map_err(corrupt)?;
        let revision: u64 = row.get(1).map_err(corrupt)?;
        let task: String = row.get(2).map_err(corrupt)?;
        let hash: String = row.get(3).map_err(corrupt)?;
        let observation: String = row.get(4).map_err(corrupt)?;
        let result: String = row.get(5).map_err(corrupt)?;
        let at: u64 = row.get(6).map_err(corrupt)?;
        let observation: IdleObservation = decode(&observation)?;
        let result: IdleReflection = decode(&result)?;
        checked_scope(&scope)?;
        validate_id(&task).map_err(corrupt)?;
        observation.validate().map_err(corrupt)?;
        result.validate().map_err(corrupt)?;
        let current_revision: u64 = db
            .query_row("SELECT revision FROM minds WHERE scope=?1", [&scope], |r| {
                r.get(0)
            })
            .map_err(corrupt)?;
        if at == 0
            || observation.revision.checked_add(1) != Some(revision)
            || revision > current_revision
            || scope_key(&observation.scope).map_err(corrupt)? != scope
            || fingerprint(&observation, &result).map_err(corrupt)? != hash
        {
            return Err(SubjectError::CorruptState);
        }
    }
    Ok(())
}

fn validate_autonomy(db: &Connection) -> SubjectResult<()> {
    let mut authorizations = db
        .prepare("SELECT scope,authorization_id,digest,at_ms FROM host_authorizations")
        .map_err(corrupt)?;
    let mut rows = authorizations.query([]).map_err(corrupt)?;
    while let Some(row) = rows.next().map_err(corrupt)? {
        let scope: String = row.get(0).map_err(corrupt)?;
        let authorization: String = row.get(1).map_err(corrupt)?;
        let digest: String = row.get(2).map_err(corrupt)?;
        let at: u64 = row.get(3).map_err(corrupt)?;
        checked_scope(&scope)?;
        validate_id(&authorization).map_err(corrupt)?;
        if at == 0 || digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SubjectError::CorruptState);
        }
    }
    for query in [
        "SELECT scope,problem,focus,at_ms FROM autonomy_problems",
        "SELECT scope,task,outcome,at_ms FROM reconciled_tasks",
    ] {
        let mut statement = db.prepare(query).map_err(corrupt)?;
        let mut rows = statement.query([]).map_err(corrupt)?;
        while let Some(row) = rows.next().map_err(corrupt)? {
            let scope: String = row.get(0).map_err(corrupt)?;
            let first: String = row.get(1).map_err(corrupt)?;
            let second: String = row.get(2).map_err(corrupt)?;
            let at: u64 = row.get(3).map_err(corrupt)?;
            checked_scope(&scope)?;
            validate_id(&first).map_err(corrupt)?;
            validate_id(&second).map_err(corrupt)?;
            if at == 0 {
                return Err(SubjectError::CorruptState);
            }
        }
    }
    Ok(())
}
