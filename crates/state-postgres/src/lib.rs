//! 本地 PostgreSQL 字节状态后端；单连接、单宿主锁、失败后关闭且不自动重连。
//!
//! StateStore 是同步契约。专用线程独占同步 postgres Client，避免在宿主 Tokio
//! 任务中嵌套驱动客户端 runtime。事务提交报错时可能已经落库，不能假定旧值未变。
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use postgres::{Client, Config, NoTls};
use serde::Deserialize;
use std::{
    net::IpAddr,
    sync::{Mutex, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

const LOCK_KEY: i64 = 0x4556_4553_5441_5445;
const UNAVAILABLE: &str =
    "PostgreSQL 状态后端已关闭；提交结果可能未知，必须停止宿主并检查持久化状态。";

/// 由宿主从独立凭据文件读取，不实现 Debug，字段不进入错误或日志。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionOptions {
    pub hostname: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl ConnectionOptions {
    fn config(&self) -> PluginResult<Config> {
        let host = if self.hostname == "localhost" {
            "127.0.0.1"
        } else {
            &self.hostname
        };
        let socket = cfg!(unix) && host.starts_with('/');
        if (!socket && !host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
            || self.port == 0
            || [&self.hostname, &self.database, &self.user, &self.password]
                .iter()
                .any(|value| value.trim().is_empty() || value.contains('\0'))
        {
            return Err(failure(
                "PostgreSQL 连接配置无效；当前只支持本机 loopback 或 Unix socket。",
            ));
        }
        let mut config = Config::new();
        config
            .host(host)
            .port(self.port)
            .dbname(&self.database)
            .user(&self.user)
            .password(&self.password)
            .connect_timeout(Duration::from_secs(5))
            .application_name("eve-state-postgres")
            .options("-c statement_timeout=5000 -c lock_timeout=5000")
            .keepalives_idle(Duration::from_secs(5))
            .keepalives_interval(Duration::from_secs(2))
            .keepalives_retries(2)
            .notice_callback(|_| {});
        #[cfg(target_os = "linux")]
        config.tcp_user_timeout(Duration::from_secs(5));
        Ok(config)
    }
}

enum Request {
    Get(
        Vec<u8>,
        Vec<u8>,
        mpsc::SyncSender<PluginResult<Option<Vec<u8>>>>,
    ),
    Set(
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        mpsc::SyncSender<PluginResult<()>>,
    ),
    Close,
}

struct Worker {
    sender: mpsc::SyncSender<Request>,
    thread: Option<JoinHandle<()>>,
    closed: bool,
}

/// 一个实例持有该数据库唯一的 Eve session advisory lock，使用 Arc 共享。
/// 不缓存 get，不暴露客户端，也不会在断连后重建连接或假定提交失败可安全重试。
pub struct PostgresStateStore {
    worker: Mutex<Worker>,
    read_only: bool,
}

impl PostgresStateStore {
    pub fn connect(options: ConnectionOptions) -> PluginResult<Self> {
        Self::connect_mode(options, false)
    }

    /// 只读打开已有 schema；不会初始化、修复或改写状态。
    /// 仍独占数据库宿主锁，set 被拒绝后连接继续支持读取。
    pub fn connect_read_only(options: ConnectionOptions) -> PluginResult<Self> {
        Self::connect_mode(options, true)
    }

    fn connect_mode(options: ConnectionOptions, read_only: bool) -> PluginResult<Self> {
        let mut config = options.config()?;
        if read_only {
            config.options(
                "-c statement_timeout=5000 -c lock_timeout=5000 -c default_transaction_read_only=on",
            );
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let (startup, ready) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("eve-postgres-state".into())
            .spawn(move || {
                let mut client = match config.connect(NoTls) {
                    Ok(client) => client,
                    Err(_) => {
                        let _ = startup
                            .send(Err(failure("PostgreSQL 连接失败；凭据与连接信息未记录。")));
                        return;
                    }
                };
                if let Err(error) = initialize(&mut client, !read_only) {
                    let _ = startup.send(Err(error));
                    return;
                }
                if startup.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(request) = receiver.recv() {
                    match request {
                        Request::Get(namespace, key, reply) => {
                            let result = read(&mut client, &namespace, &key);
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed {
                                break;
                            }
                        }
                        Request::Set(namespace, key, value, reply) => {
                            let result = write(&mut client, &namespace, &key, &value);
                            let failed = result.is_err();
                            let _ = reply.send(result);
                            if failed {
                                break;
                            }
                        }
                        Request::Close => break,
                    }
                }
                // Drop Client 在专用线程内执行，关闭 session 并释放 advisory lock。
            })
            .map_err(|_| failure("PostgreSQL 状态线程无法启动。"))?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                worker: Mutex::new(Worker {
                    sender,
                    thread: Some(thread),
                    closed: false,
                }),
                read_only,
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(failure(UNAVAILABLE))
            }
        }
    }

    fn request<T>(
        &self,
        build: impl FnOnce(mpsc::SyncSender<PluginResult<T>>) -> Request,
    ) -> PluginResult<T> {
        let mut worker = self.worker.lock().map_err(|_| failure(UNAVAILABLE))?;
        if worker.closed {
            return Err(failure(UNAVAILABLE));
        }
        let (reply, response) = mpsc::sync_channel(1);
        if worker.sender.send(build(reply)).is_err() {
            worker.closed = true;
            return Err(failure(UNAVAILABLE));
        }
        let result = response
            .recv()
            .unwrap_or_else(|_| Err(failure(UNAVAILABLE)));
        if result.is_err() {
            worker.closed = true;
        }
        result
    }
}

impl Drop for PostgresStateStore {
    fn drop(&mut self) {
        let worker = self
            .worker
            .get_mut()
            .unwrap_or_else(|error| error.into_inner());
        let _ = worker.sender.send(Request::Close);
        if let Some(thread) = worker.thread.take() {
            let _ = thread.join();
        }
    }
}

impl StateStore for PostgresStateStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.request(|reply| {
            Request::Get(
                namespace.as_str().as_bytes().to_vec(),
                key.as_bytes().to_vec(),
                reply,
            )
        })
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.read_only {
            return Err(failure("PostgreSQL 只读状态后端拒绝写入。"));
        }
        self.request(|reply| {
            Request::Set(
                namespace.as_str().as_bytes().to_vec(),
                key.into_bytes(),
                value,
                reply,
            )
        })
    }
}

fn failure(message: &'static str) -> PluginError {
    PluginError::State(message.into())
}

fn initialize(client: &mut Client, allow_initialize: bool) -> PluginResult<()> {
    let acquired: bool = client
        .query_one("SELECT pg_catalog.pg_try_advisory_lock($1)", &[&LOCK_KEY])
        .and_then(|row| row.try_get(0))
        .map_err(|_| failure("PostgreSQL 宿主锁检查失败。"))?;
    if !acquired {
        return Err(failure("PostgreSQL 数据库已有 Eve 宿主占用。"));
    }
    let mut transaction = client
        .transaction()
        .map_err(|_| failure("PostgreSQL 初始化事务失败。"))?;
    let exists: bool = transaction
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = 'eve_state')",
            &[],
        )
        .and_then(|row| row.try_get(0))
        .map_err(|_| failure("PostgreSQL 状态 schema 检查失败。"))?;
    if !exists {
        if !allow_initialize {
            return Err(failure(
                "PostgreSQL 状态 schema 不存在；只读后端不会初始化新状态。",
            ));
        }
        transaction.batch_execute("CREATE SCHEMA eve_state;
            CREATE TABLE eve_state.metadata (singleton BOOLEAN PRIMARY KEY CHECK (singleton), format_version INTEGER NOT NULL);
            INSERT INTO eve_state.metadata (singleton,format_version) VALUES (true,1);
            CREATE TABLE eve_state.entries (namespace BYTEA NOT NULL, key BYTEA NOT NULL, value BYTEA NOT NULL, PRIMARY KEY(namespace,key));")
            .map_err(|_| failure("PostgreSQL 状态 schema 创建失败。"))?;
    } else {
        let versions = transaction
            .query(
                "SELECT singleton,format_version FROM eve_state.metadata",
                &[],
            )
            .map_err(|_| failure("PostgreSQL 状态 schema 无效；未自动修复。"))?;
        if versions.len() != 1
            || versions[0].try_get::<_, bool>(0).ok() != Some(true)
            || versions[0].try_get::<_, i32>(1).ok() != Some(1)
        {
            return Err(failure("PostgreSQL 状态版本未知或损坏；未改写数据。"));
        }
        let columns = transaction.query("SELECT column_name,udt_name,is_nullable FROM information_schema.columns WHERE table_schema='eve_state' AND table_name='entries' ORDER BY ordinal_position", &[])
            .map_err(|_| failure("PostgreSQL 状态表检查失败。"))?;
        let shape = columns
            .iter()
            .map(|row| {
                (
                    row.try_get::<_, String>(0).ok(),
                    row.try_get::<_, String>(1).ok(),
                    row.try_get::<_, String>(2).ok(),
                )
            })
            .collect::<Vec<_>>();
        let expected = ["namespace", "key", "value"]
            .map(|name| (Some(name.into()), Some("bytea".into()), Some("NO".into())));
        if shape != expected {
            return Err(failure("PostgreSQL 状态表结构不匹配；未自动修复。"));
        }
        let primary: bool = transaction.query_one("SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_constraint WHERE conrelid='eve_state.entries'::regclass AND contype='p' AND conkey=ARRAY[1,2]::smallint[])", &[])
            .and_then(|row| row.try_get(0)).map_err(|_| failure("PostgreSQL 状态主键检查失败。"))?;
        if !primary {
            return Err(failure("PostgreSQL 状态主键不匹配；未自动修复。"));
        }
    }
    transaction
        .commit()
        .map_err(|_| failure("PostgreSQL 初始化提交结果未知；请检查状态。"))
}

fn read(client: &mut Client, namespace: &[u8], key: &[u8]) -> PluginResult<Option<Vec<u8>>> {
    client
        .query_opt(
            "SELECT value FROM eve_state.entries WHERE namespace=$1 AND key=$2",
            &[&namespace, &key],
        )
        .and_then(|row| row.map(|row| row.try_get(0)).transpose())
        .map_err(|_| failure(UNAVAILABLE))
}

fn write(client: &mut Client, namespace: &[u8], key: &[u8], value: &[u8]) -> PluginResult<()> {
    let mut transaction = client.transaction().map_err(|_| failure(UNAVAILABLE))?;
    transaction.execute("INSERT INTO eve_state.entries(namespace,key,value) VALUES($1,$2,$3) ON CONFLICT(namespace,key) DO UPDATE SET value=EXCLUDED.value", &[&namespace,&key,&value])
        .map_err(|_| failure(UNAVAILABLE))?;
    transaction.commit().map_err(|_| failure(UNAVAILABLE))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_required_and_remote_no_tls_is_rejected_without_echoing_values() {
        let mut options: ConnectionOptions = serde_json::from_str(r#"{"hostname":"REMOTE_PRIVATE_HOST","port":5432,"database":"PRIVATE_DATABASE","user":"PRIVATE_USER","password":"PRIVATE_PASSWORD"}"#).unwrap();
        let error = options.config().err().unwrap().to_string();
        for private in [
            "REMOTE_PRIVATE_HOST",
            "PRIVATE_DATABASE",
            "PRIVATE_USER",
            "PRIVATE_PASSWORD",
        ] {
            assert!(!error.contains(private));
        }
        options.hostname = "localhost".into();
        assert!(options.config().is_ok());
        options.password.clear();
        assert!(options.config().is_err());
        assert!(serde_json::from_str::<ConnectionOptions>(r#"{"hostname":"localhost"}"#).is_err());
    }
}
