//! 组合层选择文件或本地 SQL 状态；目录和数据库绑定都由宿主持有。
use crate::AppError;
use eve_kernel::backends::FileStateStore;
use eve_plugin_api::PluginId;
use eve_state_postgres::{ConnectionOptions, PostgresStateStore};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::Arc,
};

#[derive(Serialize, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DatabaseBinding {
    version: u8,
    hostname: String,
    port: u16,
    database: String,
    user: String,
}

struct DatabaseStore {
    // 保留同 state-dir 的本地排他锁；数据库连接另有跨目录的数据库级排他锁。
    postgres: PostgresStateStore,
    _directory: FileStateStore,
}

impl eve_plugin_api::StateStore for DatabaseStore {
    fn get(
        &self,
        namespace: &PluginId,
        key: &str,
    ) -> eve_plugin_api::PluginResult<Option<Vec<u8>>> {
        self.postgres.get(namespace, key)
    }
    fn set(
        &self,
        namespace: &PluginId,
        key: String,
        value: Vec<u8>,
    ) -> eve_plugin_api::PluginResult<()> {
        self.postgres.set(namespace, key, value)
    }
}

fn read_private_file(path: &std::path::Path) -> Result<Vec<u8>, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "数据库配置或绑定文件不可读。")?;
    if !metadata.file_type().is_file() || metadata.len() > 16_384 {
        return Err("数据库配置或绑定必须是至多16KiB的普通文件。".into());
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| "数据库配置或绑定文件不可读。")?
        .take(16_385)
        .read_to_end(&mut bytes)
        .map_err(|_| "数据库配置或绑定读取失败。")?;
    if bytes.len() > 16_384 {
        return Err("数据库配置或绑定文件超出上限。".into());
    }
    Ok(bytes)
}

pub(crate) fn open_state_store(
    directory_path: &Path,
    database_config: Option<&Path>,
) -> Result<Arc<dyn eve_plugin_api::StateStore>, AppError> {
    let directory = FileStateStore::open(directory_path)?;
    let marker = directory.directory().join("state.backend.json");
    let marker_exists = match fs::symlink_metadata(&marker) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err("无法检查状态后端绑定，未切换后端。".into()),
    };
    let Some(path) = database_config else {
        if marker_exists {
            return Err(
                "该目录已绑定 PostgreSQL；必须继续提供 --database-config，未切换为空文件库。"
                    .into(),
            );
        }
        return Ok(Arc::new(directory));
    };
    match fs::symlink_metadata(directory.directory().join("state.json")) {
        Ok(_) => {
            return Err(
                "该目录已有文件状态，不能静默切换数据库；请显式选择新的 --state-dir。".into(),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("无法检查原文件状态，未切换数据库。".into()),
    }
    let connection: ConnectionOptions = serde_json::from_slice(&read_private_file(path)?).map_err(
        |_| "数据库配置JSON无效；需要hostname、port、database、user、password，拒绝额外字段。",
    )?;
    let binding = DatabaseBinding {
        version: 1,
        hostname: connection.hostname.clone(),
        port: connection.port,
        database: connection.database.clone(),
        user: connection.user.clone(),
    };
    if marker_exists {
        let saved: DatabaseBinding = serde_json::from_slice(&read_private_file(&marker)?)
            .map_err(|_| "状态后端绑定损坏或版本未知，未覆盖。")?;
        if saved != binding {
            return Err("数据库目标与该目录的持久化绑定不一致，未切换数据库。".into());
        }
    }
    let postgres = PostgresStateStore::connect(connection)?;
    if !marker_exists {
        let mut file_options = OpenOptions::new();
        file_options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file_options.mode(0o600);
        }
        let mut file = file_options
            .open(&marker)
            .map_err(|_| "状态后端绑定创建失败。")?;
        file.write_all(&serde_json::to_vec(&binding)?)
            .map_err(|_| "状态后端绑定保存失败。")?;
        file.sync_all().map_err(|_| "状态后端绑定同步失败。")?;
    }
    Ok(Arc::new(DatabaseStore {
        postgres,
        _directory: directory,
    }))
}
