//! 单宿主 JSON 状态后端：整份快照原子替换，提交成功后才更新内存。

use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor},
};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const VERSION: u32 = 1;
type Entries = BTreeMap<String, BTreeMap<String, Vec<u8>>>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    version: u32,
    #[serde(deserialize_with = "unique_entries")]
    entries: Entries,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: BTreeMap::new(),
        }
    }
}

/// 避免重复 namespace/key 被 serde 的普通 Map 静默覆盖。
struct UniqueMap<V>(BTreeMap<String, V>);

impl<'de, V: Deserialize<'de>> Deserialize<'de> for UniqueMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueVisitor<V>(PhantomData<V>);
        impl<'de, V: Deserialize<'de>> Visitor<'de> for UniqueVisitor<V> {
            type Value = UniqueMap<V>;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("不含重复键的状态对象")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    if entries.insert(key, value).is_some() {
                        return Err(de::Error::custom("状态对象包含重复键"));
                    }
                }
                Ok(UniqueMap(entries))
            }
        }
        deserializer.deserialize_map(UniqueVisitor(PhantomData))
    }
}

fn unique_entries<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Entries, D::Error> {
    let map = UniqueMap::<UniqueMap<Vec<u8>>>::deserialize(deserializer)?;
    Ok(map
        .0
        .into_iter()
        .map(|(namespace, values)| (namespace, values.0))
        .collect())
}

/// 每个数据目录独占一个实例，通过 Arc 在同一宿主内共享。
/// set 的成功表示快照文件已同步并替换，不保证断电后的目录项持久性。
pub struct FileStateStore {
    directory: PathBuf,
    snapshot: Mutex<Snapshot>,
    // 锁文件不删除；删除会让另一个实例锁住不同文件，破坏目录排他性。
    _lock: File,
}

impl FileStateStore {
    pub fn open(directory: impl AsRef<Path>) -> PluginResult<Self> {
        fs::create_dir_all(directory.as_ref()).map_err(|error| failure("创建状态目录", error))?;
        // 固定绝对目录，后续切换进程工作目录不改变实际写入位置。
        let directory = directory
            .as_ref()
            .canonicalize()
            .map_err(|error| failure("解析状态目录", error))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("state.lock"))
            .map_err(|error| failure("打开状态锁", error))?;
        lock.try_lock()
            .map_err(|error| failure("状态目录被占用或无法加锁", error))?;

        let snapshot_path = directory.join("state.json");
        // 先独立解析快照；失败时在返回错误前显式释放目录锁，保证调用方
        // 可以在同一进程中修复文件后立即重试打开。
        let snapshot = (|| -> PluginResult<Snapshot> {
            // read 会跟随链接；悬空链接的 NotFound 不能被解释为新建空库。
            match fs::symlink_metadata(&snapshot_path) {
                Ok(metadata) => {
                    if !metadata.file_type().is_file() {
                        return Err(PluginError::State(
                            "状态快照必须是普通文件，不能是链接或目录".into(),
                        ));
                    }
                    let bytes =
                        fs::read(&snapshot_path).map_err(|error| failure("打开状态快照", error))?;
                    let snapshot: Snapshot = serde_json::from_slice(&bytes)
                        .map_err(|error| failure("读取状态快照", error))?;
                    if snapshot.version != VERSION {
                        return Err(PluginError::State(format!(
                            "不支持状态版本 {}，需要版本 {VERSION}",
                            snapshot.version
                        )));
                    }
                    for namespace in snapshot.entries.keys() {
                        PluginId::new(namespace.clone())
                            .map_err(|_| PluginError::State("状态包含空插件标识".into()))?;
                    }
                    Ok(snapshot)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(Snapshot::default())
                }
                Err(error) => Err(failure("打开状态快照", error)),
            }
        })();
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                drop(lock);
                return Err(error);
            }
        };
        Ok(Self {
            directory,
            snapshot: Mutex::new(snapshot),
            _lock: lock,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn commit(&self, snapshot: &Snapshot) -> PluginResult<()> {
        let bytes = serde_json::to_vec(snapshot).map_err(|error| failure("编码状态快照", error))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".state-")
            .suffix(".tmp")
            .tempfile_in(&self.directory)
            .map_err(|error| failure("创建状态临时文件", error))?;
        temporary
            .write_all(&bytes)
            .map_err(|error| failure("写入状态临时文件", error))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| failure("同步状态临时文件", error))?;
        // 唯一提交点。绝不先删除旧快照；失败时尝试清理临时文件并保留旧文件。
        temporary
            .persist(self.directory.join("state.json"))
            .map_err(|error| failure("提交状态快照", error.error))?;
        // 提交后不再执行可能返回 Err 的 I/O，避免磁盘已更新却报告未提交。
        Ok(())
    }
}

impl StateStore for FileStateStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        let snapshot = self
            .snapshot
            .lock()
            .map_err(|_| PluginError::State("状态快照锁中毒".into()))?;
        Ok(snapshot
            .entries
            .get(namespace.as_str())
            .and_then(|values| values.get(key))
            .cloned())
    }

    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| PluginError::State("状态快照锁中毒".into()))?;
        let mut candidate = snapshot.clone();
        candidate
            .entries
            .entry(namespace.as_str().into())
            .or_default()
            .insert(key, value);
        self.commit(&candidate)?;
        *snapshot = candidate;
        Ok(())
    }
}

fn failure(operation: &str, error: impl fmt::Display) -> PluginError {
    PluginError::State(format!("{operation}失败：{error}"))
}
