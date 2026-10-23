//! 单宿主、用户与会话隔离的子代理配置；不注册模型权限，不执行任务。
use eve_llm_api::ContextScope;
use eve_task_api::{
    SubagentProfile, SubagentProfileDraft, SubagentProfileError as Error, SubagentProfiles,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
pub struct SqliteSubagentProfiles {
    database: Mutex<Connection>,
    healthy: AtomicBool,
}
fn id(raw: &str) -> Result<u64, Error> {
    raw.strip_prefix("agent-")
        .and_then(|n| n.parse::<u64>().ok())
        .filter(|n| *n > 0 && *n <= i64::MAX as u64)
        .ok_or(Error::InvalidInput)
}
fn valid_scope(scope: &ContextScope) -> Result<(), Error> {
    if scope.session_id.is_empty()
        || scope.user_id.is_empty()
        || scope.session_id.len() > 512
        || scope.user_id.len() > 256
    {
        Err(Error::InvalidInput)
    } else {
        Ok(())
    }
}
fn row(sequence: u64, payload: String) -> Result<SubagentProfile, Error> {
    // JSON 转义可能让合法的 8 KiB 角色说明膨胀至六倍，工具名称另占空间。
    if payload.len() > 128 * 1024 {
        return Err(Error::CorruptState);
    }
    let profile: SubagentProfileDraft =
        serde_json::from_str(&payload).map_err(|_| Error::CorruptState)?;
    profile.validate().map_err(|_| Error::CorruptState)?;
    Ok(SubagentProfile {
        id: format!("agent-{sequence}"),
        revision: 1,
        profile,
    })
}
impl SqliteSubagentProfiles {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if path
            .symlink_metadata()
            .is_ok_and(|m| !m.file_type().is_file())
        {
            return Err(Error::CorruptState);
        }
        let exists = path.exists();
        let database = Connection::open(path).map_err(|_| Error::CorruptState)?;
        database
            .busy_timeout(Duration::from_millis(100))
            .map_err(|_| Error::Storage)?;
        if !exists {
            database.execute_batch("BEGIN IMMEDIATE; CREATE TABLE metadata(version INTEGER NOT NULL); INSERT INTO metadata VALUES(1);
    CREATE TABLE profiles(seq INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL,user_id TEXT NOT NULL,name TEXT NOT NULL,payload TEXT NOT NULL,UNIQUE(session_id,user_id,name));
    CREATE INDEX scoped_profiles ON profiles(session_id,user_id,seq); COMMIT;").map_err(|_|Error::Storage)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| Error::Storage)?;
            }
        }
        let version: Vec<u64> = database
            .prepare("SELECT version FROM metadata")
            .map_err(|_| Error::CorruptState)?
            .query_map([], |r| r.get(0))
            .map_err(|_| Error::CorruptState)?
            .collect::<Result<_, _>>()
            .map_err(|_| Error::CorruptState)?;
        let check: String = database
            .query_row("PRAGMA quick_check", [], |r| r.get(0))
            .map_err(|_| Error::CorruptState)?;
        if version != [1] || check != "ok" {
            return Err(Error::CorruptState);
        }
        database
            .prepare("SELECT seq,session_id,user_id,name,payload FROM profiles LIMIT 0")
            .map_err(|_| Error::CorruptState)?;
        database
            .pragma_update(None, "journal_mode", "DELETE")
            .map_err(|_| Error::Storage)?;
        database
            .pragma_update(None, "synchronous", "FULL")
            .map_err(|_| Error::Storage)?;
        Ok(Self {
            database: Mutex::new(database),
            healthy: AtomicBool::new(true),
        })
    }
    fn run<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T, Error>) -> Result<T, Error> {
        if !self.healthy.load(Ordering::SeqCst) {
            return Err(Error::Storage);
        }
        let result = self
            .database
            .lock()
            .map_err(|_| Error::Storage)
            .and_then(|mut db| f(&mut db));
        if matches!(&result, Err(Error::Storage | Error::CorruptState)) {
            self.healthy.store(false, Ordering::SeqCst);
        }
        result
    }
}
impl SubagentProfiles for SqliteSubagentProfiles {
    fn create(
        &self,
        scope: &ContextScope,
        draft: SubagentProfileDraft,
    ) -> Result<SubagentProfile, Error> {
        valid_scope(scope)?;
        draft.validate()?;
        let payload = serde_json::to_string(&draft).map_err(|_| Error::InvalidInput)?;
        self.run(|db|{
   let tx=db.transaction().map_err(|_|Error::Storage)?;
   tx.execute("INSERT OR IGNORE INTO profiles(session_id,user_id,name,payload) VALUES(?1,?2,?3,?4)",params![scope.session_id,scope.user_id,draft.name,payload]).map_err(|_|Error::Storage)?;
   let (seq,actual):(u64,String)=tx.query_row("SELECT seq,payload FROM profiles WHERE session_id=?1 AND user_id=?2 AND name=?3",params![scope.session_id,scope.user_id,draft.name],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|Error::Storage)?;
   if actual!=payload {return Err(Error::Conflict);}
   let result=row(seq,actual)?;tx.commit().map_err(|_|Error::Storage)?;Ok(result)
  })
    }
    fn get(&self, scope: &ContextScope, name: &str) -> Result<SubagentProfile, Error> {
        valid_scope(scope)?;
        let seq = id(name)?;
        self.run(|db| {
            db.query_row(
                "SELECT payload FROM profiles WHERE seq=?1 AND session_id=?2 AND user_id=?3",
                params![seq, scope.session_id, scope.user_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|_| Error::Storage)?
            .ok_or(Error::NotFound)
            .and_then(|raw| row(seq, raw))
        })
    }
    fn list(
        &self,
        scope: &ContextScope,
        after: Option<&str>,
    ) -> Result<Vec<SubagentProfile>, Error> {
        valid_scope(scope)?;
        let after = after.map(id).transpose()?.unwrap_or(0);
        self.run(|db|{let mut stmt=db.prepare("SELECT seq,payload FROM profiles WHERE session_id=?1 AND user_id=?2 AND seq>?3 ORDER BY seq LIMIT 64").map_err(|_|Error::Storage)?;
   stmt.query_map(params![scope.session_id,scope.user_id,after],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?))).map_err(|_|Error::Storage)?.map(|r|r.map_err(|_|Error::Storage).and_then(|(seq,raw)|row(seq,raw))).collect()
  })
    }
    fn delete(&self, scope: &ContextScope, name: &str, revision: u64) -> Result<(), Error> {
        valid_scope(scope)?;
        let seq = id(name)?;
        if revision != 1 {
            return Err(Error::Conflict);
        }
        self.run(|db| {
            if db
                .execute(
                    "DELETE FROM profiles WHERE seq=?1 AND session_id=?2 AND user_id=?3",
                    params![seq, scope.session_id, scope.user_id],
                )
                .map_err(|_| Error::Storage)?
                == 1
            {
                Ok(())
            } else {
                Err(Error::NotFound)
            }
        })
    }
}
