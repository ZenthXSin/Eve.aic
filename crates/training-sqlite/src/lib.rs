//! 不含原文的表达统计数据库；旧 JSON 只幂等导入，不删除、不改写。
use eve_llm_api::ContextScope;
use eve_plugin_api::{PluginError, PluginResult};
use eve_training_api::{ExpressionRecord, ExpressionSnapshot, ExpressionStore};
use ring::{
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use std::{
    fs,
    io::Write,
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn failed() -> PluginError {
    PluginError::State("表达统计数据库不可用；原记录保留，未清空或回退到空状态".into())
}
fn invalid() -> PluginError {
    PluginError::Task("表达统计记录无效或消息 ID 内容冲突；原记录保留".into())
}
fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn scope_valid(scope: &ContextScope) -> bool {
    [&scope.session_id, &scope.user_id].iter().all(|v| {
        !v.is_empty()
            && v.len() <= 256
            && v.trim() == v.as_str()
            && !v.chars().any(char::is_control)
    })
}

pub struct SqliteExpressions {
    connection: Mutex<Connection>,
    healthy: AtomicBool,
}
impl SqliteExpressions {
    pub fn open(path: &Path) -> PluginResult<Self> {
        let marker = path.with_extension("required");
        for p in [path, marker.as_path()] {
            if p.symlink_metadata().is_ok_and(|m| !m.file_type().is_file()) {
                return Err(failed());
            }
        }
        let exists = path.exists();
        if exists != marker.exists() {
            return Err(failed());
        }
        let db = Connection::open(path).map_err(|_| failed())?;
        db.busy_timeout(Duration::from_secs(2))
            .map_err(|_| failed())?;
        if !exists {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))
                    .map_err(|_| failed())?;
            }
            let mut identity = [0u8; 16];
            SystemRandom::new()
                .fill(&mut identity)
                .map_err(|_| failed())?;
            let identity: String = identity.iter().map(|b| format!("{b:02x}")).collect();
            db.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE metadata(version INTEGER NOT NULL, identity TEXT NOT NULL, legacy_sha TEXT NOT NULL);
                CREATE TABLE expressions(seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL, user_id TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL,
                    chars INTEGER NOT NULL CHECK(chars BETWEEN 0 AND 512),
                    paragraphs INTEGER NOT NULL CHECK(paragraphs BETWEEN 0 AND 512),
                    question INTEGER NOT NULL CHECK(question IN (0,1)),
                    formal INTEGER NOT NULL CHECK(formal IN (0,1)), casual INTEGER NOT NULL CHECK(casual IN (0,1)),
                    active INTEGER NOT NULL CHECK(active IN (0,1)), UNIQUE(session_id,user_id,id));
                CREATE INDEX scope_expression ON expressions(session_id,user_id,active,chars);
                CREATE INDEX active_expression ON expressions(active,chars);
                COMMIT;").map_err(|_| failed())?;
            db.execute("INSERT INTO metadata VALUES(1,?1,'')", [&identity])
                .map_err(|_| failed())?;
            let parent = path.parent().ok_or_else(failed)?;
            let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| failed())?;
            file.write_all(identity.as_bytes())
                .and_then(|_| file.as_file().sync_all())
                .map_err(|_| failed())?;
            file.persist_noclobber(&marker).map_err(|_| failed())?;
            #[cfg(unix)]
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|_| failed())?;
        }
        let values: Vec<(u64, String)> = db
            .prepare("SELECT version,identity FROM metadata")
            .map_err(|_| failed())?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|_| failed())?
            .collect::<Result<_, _>>()
            .map_err(|_| failed())?;
        if values.len() != 1
            || values[0].0 != 1
            || values[0].1.len() != 32
            || fs::read_to_string(&marker).map_err(|_| failed())? != values[0].1
        {
            return Err(failed());
        }
        let check: String = db
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|_| failed())?;
        if check != "ok" {
            return Err(failed());
        }
        db.prepare("SELECT session_id,user_id,id,body,chars,paragraphs,question,formal,casual,active FROM expressions LIMIT 0").map_err(|_| failed())?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")
            .map_err(|_| failed())?;
        Ok(Self {
            connection: Mutex::new(db),
            healthy: AtomicBool::new(true),
        })
    }
    fn run<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> PluginResult<T>,
    ) -> PluginResult<T> {
        if !self.healthy.load(Ordering::Acquire) {
            return Err(failed());
        }
        let mut connection = self.connection.lock().map_err(|_| failed())?;
        let result = operation(&mut connection);
        if result
            .as_ref()
            .is_err_and(|e| matches!(e, PluginError::State(_)))
        {
            self.healthy.store(false, Ordering::Release);
        }
        result
    }
}
fn record(row: &Row<'_>) -> rusqlite::Result<ExpressionRecord> {
    Ok(ExpressionRecord {
        scope: ContextScope {
            session_id: row.get(0)?,
            user_id: row.get(1)?,
        },
        id: row.get(2)?,
        body: row.get(3)?,
        chars: row.get(4)?,
        paragraphs: row.get(5)?,
        question: row.get(6)?,
        formal: row.get(7)?,
        casual: row.get(8)?,
        active: row.get(9)?,
    })
}
fn get(db: &Connection, scope: &ContextScope, id: &str) -> PluginResult<Option<ExpressionRecord>> {
    let row=db.query_row("SELECT session_id,user_id,id,body,chars,paragraphs,question,formal,casual,active FROM expressions WHERE session_id=?1 AND user_id=?2 AND id=?3",
        params![scope.session_id,scope.user_id,id],record).optional().map_err(|_| failed())?;
    if row.as_ref().is_some_and(|r| !r.valid()) {
        return Err(failed());
    }
    Ok(row)
}
fn insert(tx: &Transaction<'_>, row: &ExpressionRecord) -> PluginResult<()> {
    tx.execute("INSERT INTO expressions(session_id,user_id,id,body,chars,paragraphs,question,formal,casual,active) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![row.scope.session_id,row.scope.user_id,row.id,row.body,row.chars,row.paragraphs,row.question,row.formal,row.casual,row.active])
        .map_err(|_| failed())?;
    Ok(())
}
impl ExpressionStore for SqliteExpressions {
    fn import_legacy(&self, rows: &[ExpressionRecord]) -> PluginResult<()> {
        if rows.iter().any(|r| !r.valid()) {
            return Err(invalid());
        }
        let digest = hash(&serde_json::to_vec(rows).map_err(|_| invalid())?);
        self.run(|db| {
            let previous:String=db.query_row("SELECT legacy_sha FROM metadata",[],|row|row.get(0)).map_err(|_| failed())?;
            if previous==digest {return Ok(());}
            let tx=db.transaction().map_err(|_| failed())?;
            for row in rows {
                if let Some(mut old)=get(&tx,&row.scope,&row.id)? {
                    let active=old.active;old.active=row.active;
                    if old!=*row {return Err(invalid());}
                    // 重置后的去重凭据保留；旧 JSON 的历史 active=true 不能复活它。
                    if active && !row.active {tx.execute("UPDATE expressions SET active=0 WHERE session_id=?1 AND user_id=?2 AND id=?3",params![row.scope.session_id,row.scope.user_id,row.id]).map_err(|_| failed())?;}
                } else {insert(&tx,row)?;}
            }
            tx.execute("UPDATE metadata SET legacy_sha=?1",[digest]).map_err(|_| failed())?;
            tx.commit().map_err(|_| failed())
        })
    }
    fn get(&self, scope: &ContextScope, id: &str) -> PluginResult<Option<ExpressionRecord>> {
        if !scope_valid(scope) || id.len() != 64 {
            return Err(invalid());
        }
        self.run(|db| get(db, scope, id))
    }
    fn insert(&self, row: &ExpressionRecord) -> PluginResult<bool> {
        if !row.valid() {
            return Err(invalid());
        }
        self.run(|db| {
            let tx = db.transaction().map_err(|_| failed())?;
            if let Some(existing) = get(&tx, &row.scope, &row.id)? {
                return if existing == *row {
                    Ok(false)
                } else {
                    Err(invalid())
                };
            }
            insert(&tx, row)?;
            tx.commit().map_err(|_| failed())?;
            Ok(true)
        })
    }
    fn snapshot(&self, scope: Option<&ContextScope>) -> PluginResult<Option<ExpressionSnapshot>> {
        if scope.is_some_and(|s| !scope_valid(s)) {
            return Err(invalid());
        }
        self.run(|db| {
            let filter=if scope.is_some(){"active=1 AND session_id=?1 AND user_id=?2"}else{"active=1"};
            let sql=format!("SELECT chars,COUNT(*),SUM(paragraphs=1),SUM(question),SUM(formal),SUM(casual),MAX(seq) FROM expressions WHERE {filter} GROUP BY chars ORDER BY chars");
            let mut statement=db.prepare(&sql).map_err(|_| failed())?;
            let params:Vec<&str>=scope.map_or_else(Vec::new,|s|vec![s.session_id.as_str(),s.user_id.as_str()]);
            let buckets:Vec<[usize;7]>=statement.query_map(rusqlite::params_from_iter(params),|r| Ok([r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?])).map_err(|_| failed())?
                .collect::<Result<_,_>>().map_err(|_| failed())?;
            let samples:usize=buckets.iter().map(|r|r[1]).sum();if samples==0{return Ok(None);}
            let nth=|n:usize| {let mut count=0;for b in &buckets {count+=b[1];if count>n{return b[0];}}0};
            let percent=|column:usize|buckets.iter().map(|r|r[column]).sum::<usize>()*100/samples;
            Ok(Some(ExpressionSnapshot {revision:hash(&serde_json::to_vec(&buckets).map_err(|_| failed())?),samples,
                median_chars:nth((samples-1)/2),p75_chars:nth((samples-1)*3/4),
                short_percent:buckets.iter().filter(|r|r[0]<=60).map(|r|r[1]).sum::<usize>()*100/samples,
                single_paragraph_percent:percent(2),question_percent:percent(3),formal_percent:percent(4),casual_percent:percent(5)}))
        })
    }
    fn reset(&self, scope: &ContextScope) -> PluginResult<()> {
        if !scope_valid(scope) {
            return Err(invalid());
        }
        self.run(|db| {
            db.execute(
                "UPDATE expressions SET active=0 WHERE session_id=?1 AND user_id=?2",
                params![scope.session_id, scope.user_id],
            )
            .map_err(|_| failed())?;
            Ok(())
        })
    }
}
