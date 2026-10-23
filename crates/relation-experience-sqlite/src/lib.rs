//! 单宿主的 Jev 分类经验库。范围键只留在本机，示例不是事实、指令或授权。
use eve_message_api::*;
use rusqlite::{Connection, params};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub struct SqliteRelationExperience {
    database: Mutex<Connection>,
    healthy: AtomicBool,
}
fn unavailable() -> RelationError {
    RelationError::Unavailable
}
fn grams(text: &str) -> BTreeSet<String> {
    let chars: Vec<_> = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    chars.windows(2).map(|p| p.iter().collect()).collect()
}
impl SqliteRelationExperience {
    pub fn open(path: &Path) -> Result<Self, RelationError> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(unavailable());
        }
        let exists = path.exists();
        let connection = Connection::open(path).map_err(|_| unavailable())?;
        connection
            .busy_timeout(Duration::from_millis(100))
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|_| unavailable())?;
        if !exists {
            connection.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE metadata (version INTEGER NOT NULL);
                INSERT INTO metadata VALUES (1);
                CREATE TABLE examples (seq INTEGER PRIMARY KEY, session_id TEXT NOT NULL, user_id TEXT NOT NULL,
                    message_id TEXT NOT NULL, payload TEXT NOT NULL, UNIQUE(session_id,user_id,message_id));
                CREATE INDEX scoped_examples ON examples(session_id,user_id,seq DESC);
                CREATE VIRTUAL TABLE example_search USING fts5(terms);
                COMMIT;").map_err(|_|unavailable())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| unavailable())?;
            }
        }
        let quick: String = connection
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|_| unavailable())?;
        let version: Vec<u64> = connection
            .prepare("SELECT version FROM metadata")
            .map_err(|_| unavailable())?
            .query_map([], |r| r.get(0))
            .map_err(|_| unavailable())?
            .collect::<Result<_, _>>()
            .map_err(|_| unavailable())?;
        if quick != "ok" || version != [1] {
            return Err(unavailable());
        }
        // 缺表或不兼容结构不能当作空库。
        connection
            .prepare("SELECT seq,session_id,user_id,message_id,payload FROM examples LIMIT 0")
            .map_err(|_| unavailable())?;
        connection
            .prepare("SELECT rowid,terms FROM example_search LIMIT 0")
            .map_err(|_| unavailable())?;
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| unavailable())?;
        Ok(Self {
            database: Mutex::new(connection),
            healthy: AtomicBool::new(true),
        })
    }
    fn guard<T>(&self, result: Result<T, RelationError>) -> Result<T, RelationError> {
        if result.is_err() && self.healthy.swap(false, Ordering::SeqCst) {
            eprintln!(
                "EVE_JEV_EXPERIENCE unavailable; preserved_database=true; model_judgment_continues_without_examples=true"
            );
        }
        result
    }
}
impl RelationExperienceStore for SqliteRelationExperience {
    fn recall(&self, input: &RelationInput) -> Result<Vec<RelationExample>, RelationError> {
        input
            .message
            .validate()
            .map_err(|_| RelationError::Protocol)?;
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(unavailable());
        }
        self.guard((|| {
            let query = grams(&input.message.text);
            if query.is_empty() {return Ok(vec![]);}
            let expression = query.iter().take(128).map(|g|format!("\"{g}\"")).collect::<Vec<_>>().join(" OR ");
            let database = self.database.lock().map_err(|_|unavailable())?;
            let mut statement = database.prepare("SELECT e.seq,e.payload FROM example_search JOIN examples e ON e.seq=example_search.rowid
                WHERE example_search MATCH ?1 AND e.session_id=?2 AND e.user_id=?3
                ORDER BY bm25(example_search), e.seq DESC LIMIT 128").map_err(|_|unavailable())?;
            let rows = statement.query_map(params![expression,input.message.target.session.session_id,input.message.target.session.user_id],
                |r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?))).map_err(|_|unavailable())?;
            let mut candidates = vec![];
            for row in rows {
                let (sequence, raw) = row.map_err(|_|unavailable())?;
                if raw.len()>16384 {return Err(unavailable());}
                let example: RelationExample = serde_json::from_str(&raw).map_err(|_|unavailable())?;
                example.validate()?;
                candidates.push((sequence,example));
            }
            // 同一原话与任务的矛盾分类优先保留用户明确指定，之后才是较新的模型判断。
            candidates.sort_by_key(|(sequence,e)|(e.source!=RelationExperienceSource::UserClarification,std::cmp::Reverse(*sequence)));
            let task = grams(&input.task_text);
            let mut ranked = vec![];
            let mut seen = BTreeSet::new();
            for (sequence,example) in candidates {
                if !seen.insert((example.task_text.clone(),example.message.clone())) {continue;}
                let words = grams(&example.message);
                let overlap = query.intersection(&words).count();
                let exact = example.message.trim()==input.message.text.trim();
                if !exact && overlap<2 {continue;}
                let score = if exact {10000} else {1000*overlap/(query.len()+words.len()-overlap).max(1)};
                let task_overlap = task.intersection(&grams(&example.task_text)).count().min(20);
                ranked.push((score,task_overlap,sequence,example));
            }
            ranked.sort_by(|a,b|(b.0,b.1,b.2).cmp(&(a.0,a.1,a.2)));
            Ok(ranked.into_iter().take(MAX_RELATION_EXAMPLES).map(|r|r.3).collect())
        })())
    }
    fn remember(
        &self,
        input: &RelationInput,
        decision: &RelationDecision,
        source: RelationExperienceSource,
    ) -> Result<(), RelationError> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(unavailable());
        }
        decision.validate(input)?;
        let example = RelationExample {
            task_text: input.task_text.clone(),
            message: input.message.text.clone(),
            parts: decision
                .parts
                .iter()
                .map(|p| RelationExamplePart {
                    intent: p.intent,
                    text: p
                        .span
                        .map(|s| input.message.text[s.start..s.end].to_owned()),
                })
                .collect(),
            explanation: decision.explanation.clone(),
            source,
        };
        // 超长、含取消/含混等情形不积累；正常判断与路由不因此受阻。
        if example.validate().is_err() {
            return Ok(());
        }
        let payload = serde_json::to_string(&example).map_err(|_| unavailable())?;
        if payload.len() > 16384 {
            return Ok(());
        }
        self.guard((|| {
            let mut database=self.database.lock().map_err(|_|unavailable())?;
            let transaction=database.transaction().map_err(|_|unavailable())?;
            let inserted = transaction.execute("INSERT OR IGNORE INTO examples(session_id,user_id,message_id,payload) VALUES (?1,?2,?3,?4)", params![input.message.target.session.session_id,input.message.target.session.user_id,input.message.message_id,payload]).map_err(|_|unavailable())?;
            let (sequence, actual): (u64,String) = transaction.query_row("SELECT seq,payload FROM examples WHERE session_id=?1 AND user_id=?2 AND message_id=?3",params![input.message.target.session.session_id,input.message.target.session.user_id,input.message.message_id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|unavailable())?;
            if actual!=payload {return Err(unavailable());}
            if inserted==1 {
                let terms = grams(&example.message).into_iter().collect::<Vec<_>>().join(" ");
                transaction.execute("INSERT INTO example_search(rowid,terms) VALUES (?1,?2)",params![sequence,terms]).map_err(|_|unavailable())?;
            }
            transaction.commit().map_err(|_|unavailable())?;
            Ok(())
        })())
    }
}
