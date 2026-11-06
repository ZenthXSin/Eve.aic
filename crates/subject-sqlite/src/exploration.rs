use super::*;

fn sha256(text: &str) -> String {
    digest(&SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn verify_excerpts(experience: &ExplorationExperience) -> SubjectResult<()> {
    for excerpt in experience
        .tools
        .iter()
        .flat_map(|tool| {
            std::iter::once(&tool.arguments)
                .chain(tool.output.iter())
                .chain(tool.error.iter())
        })
        .chain(experience.subjective_notes.iter())
    {
        if !excerpt.truncated && sha256(&excerpt.text) != excerpt.sha256 {
            return Err(SubjectError::InvalidInput);
        }
    }
    Ok(())
}

struct ExperienceRow {
    seq: u64,
    session_id: String,
    scope: String,
    started_at: u64,
    ended_at: u64,
    payload: String,
    digest: String,
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ExperienceRow> {
    Ok(ExperienceRow {
        seq: row.get(0)?,
        session_id: row.get(1)?,
        scope: row.get(2)?,
        started_at: row.get(3)?,
        ended_at: row.get(4)?,
        payload: row.get(5)?,
        digest: row.get(6)?,
    })
}

fn checked(row: ExperienceRow) -> SubjectResult<(u64, ExplorationExperience)> {
    if row.seq == 0
        || row.payload.len() > MAX_EXPLORATION_EXPERIENCE_BYTES
        || row.digest != sha256(&row.payload)
    {
        return Err(SubjectError::CorruptState);
    }
    let experience: ExplorationExperience = serde_json::from_str(&row.payload).map_err(corrupt)?;
    experience.validate().map_err(corrupt)?;
    verify_excerpts(&experience).map_err(corrupt)?;
    if row.session_id != experience.session_id
        || row.scope != scope_key(&experience.scope).map_err(corrupt)?
        || row.started_at != experience.started_at_ms
        || row.ended_at != experience.ended_at_ms
    {
        return Err(SubjectError::CorruptState);
    }
    Ok((row.seq, experience))
}

pub(super) fn record(
    store: &SqliteIdleMemory,
    experience: &ExplorationExperience,
) -> SubjectResult<()> {
    experience.validate()?;
    verify_excerpts(experience)?;
    let scope = scope_key(&experience.scope)?;
    let started = bounded_integer(experience.started_at_ms)?;
    let ended = bounded_integer(experience.ended_at_ms)?;
    let payload = encode(experience)?;
    let fingerprint = sha256(&payload);
    store.run(|db| {
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        // 不以范围缩窄查重；全局 session id 不能被另一个身份重新认领。
        let previous = tx
            .query_row(
                "SELECT seq,session_id,scope,started_at,ended_at,payload,digest FROM exploration_experiences WHERE session_id=?1",
                [&experience.session_id],
                row,
            )
            .optional()
            .map_err(storage)?;
        if let Some(previous) = previous {
            let (_, previous) = checked(previous)?;
            return if &previous == experience {
                Ok(())
            } else {
                Err(SubjectError::InvalidTransition)
            };
        }
        tx.execute(
            "INSERT INTO exploration_experiences(session_id,scope,started_at,ended_at,payload,digest) VALUES(?1,?2,?3,?4,?5,?6)",
            params![experience.session_id, scope, started, ended, payload, fingerprint],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)
    })
}

pub(super) fn get(
    store: &SqliteIdleMemory,
    scope: &SubjectScope,
    session: &str,
) -> SubjectResult<Option<ExplorationExperience>> {
    validate_id(session)?;
    let key = scope_key(scope)?;
    store.run(|db| get_in(db, &key, session))
}

pub(super) fn get_in(
    db: &Connection,
    key: &str,
    session: &str,
) -> SubjectResult<Option<ExplorationExperience>> {
    db.query_row(
            "SELECT seq,session_id,scope,started_at,ended_at,payload,digest FROM exploration_experiences WHERE scope=?1 AND session_id=?2",
            params![key, session],
            row,
        )
        .optional()
        .map_err(storage)?
        .map(|row| checked(row).map(|(_, experience)| experience))
        .transpose()
}

pub(super) fn page(
    store: &SqliteIdleMemory,
    scope: &SubjectScope,
    before: Option<u64>,
    limit: usize,
) -> SubjectResult<ExplorationExperiencePage> {
    if limit == 0 || limit > MAX_EXPLORATION_EXPERIENCE_PAGE || before == Some(0) {
        return Err(SubjectError::InvalidInput);
    }
    let key = scope_key(scope)?;
    let before = before.map(bounded_integer).transpose()?;
    store.run(|db| {
        let tx = db.transaction().map_err(storage)?;
        let total = tx
            .query_row(
                "SELECT count(*) FROM exploration_experiences WHERE scope=?1",
                [&key],
                |r| r.get(0),
            )
            .map_err(storage)?;
        let mut stmt = tx
            .prepare(
                "SELECT seq,session_id,scope,started_at,ended_at,payload,digest FROM exploration_experiences WHERE scope=?1 AND (?2 IS NULL OR seq<?2) ORDER BY seq DESC LIMIT ?3",
            )
            .map_err(storage)?;
        let mut records = stmt
            .query_map(params![key, before, limit as u64 + 1], row)
            .map_err(storage)?
            .map(|record| checked(record.map_err(storage)?))
            .collect::<SubjectResult<Vec<_>>>()?;
        let next_before = if records.len() > limit {
            records.pop();
            records.last().map(|(seq, _)| *seq)
        } else {
            None
        };
        Ok(ExplorationExperiencePage {
            total,
            items: records.into_iter().map(|(_, experience)| experience).collect(),
            next_before,
        })
    })
}

fn source_row(
    db: &Connection,
    scope: &str,
    reflection: &str,
) -> SubjectResult<Option<(String, u64)>> {
    let row: Option<(String, u64)> = db
        .query_row(
            "SELECT focus_id,at_ms FROM exploration_sources WHERE scope=?1 AND reflection_id=?2",
            params![scope, reflection],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage)?;
    if let Some((focus, at)) = &row
        && (validate_id(focus).is_err() || *at == 0)
    {
        return Err(SubjectError::CorruptState);
    }
    Ok(row)
}

fn committed_exploration(db: &Connection, scope: &str, reflection: &str) -> SubjectResult<bool> {
    let stored: Option<(String, String, String)> = db
        .query_row(
            "SELECT observation,result,fingerprint FROM reflections WHERE scope=?1 AND task_id=?2",
            params![scope, reflection],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(storage)?;
    let Some((observation, result, hash)) = stored else {
        return Ok(false);
    };
    let observation: IdleObservation = decode(&observation)?;
    let result: IdleReflection = decode(&result)?;
    observation.validate().map_err(corrupt)?;
    result.validate().map_err(corrupt)?;
    if scope_key(&observation.scope).map_err(corrupt)? != scope
        || fingerprint(&observation, &result).map_err(corrupt)? != hash
    {
        return Err(SubjectError::CorruptState);
    }
    Ok(result.choice == IdleChoice::Explore)
}

pub(super) fn reserve_source(
    store: &SqliteIdleMemory,
    scope: &SubjectScope,
    reflection: &str,
    focus: &str,
    now: u64,
) -> SubjectResult<()> {
    validate_id(reflection)?;
    validate_id(focus)?;
    if now == 0 {
        return Err(SubjectError::InvalidInput);
    }
    let at = bounded_integer(now)?;
    let key = scope_key(scope)?;
    store.run(|db| {
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage)?;
        if !committed_exploration(&tx, &key, reflection)? {
            return Err(SubjectError::InvalidTransition);
        }
        if let Some((old, _)) = source_row(&tx, &key, reflection)? {
            return if old == focus {
                Ok(())
            } else {
                Err(SubjectError::InvalidTransition)
            };
        }
        let taken: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM exploration_sources WHERE scope=?1 AND focus_id=?2)",
                params![key, focus],
                |r| r.get(0),
            )
            .map_err(storage)?;
        if taken {
            return Err(SubjectError::InvalidTransition);
        }
        tx.execute(
            "INSERT INTO exploration_sources(scope,reflection_id,focus_id,at_ms) VALUES(?1,?2,?3,?4)",
            params![key, reflection, focus, at],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)
    })
}

pub(super) fn source(
    store: &SqliteIdleMemory,
    scope: &SubjectScope,
    reflection: &str,
) -> SubjectResult<Option<String>> {
    validate_id(reflection)?;
    let key = scope_key(scope)?;
    store.run(|db| {
        let stored = source_row(db, &key, reflection)?;
        if stored.is_some() && !committed_exploration(db, &key, reflection)? {
            return Err(SubjectError::CorruptState);
        }
        Ok(stored.map(|(focus, _)| focus))
    })
}
