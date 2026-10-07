use crate::ConfigBootstrap;
use eve_config_api::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 1;
static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);
type Snapshots = BTreeMap<String, ConfigSnapshot>;

fn panel_error(error: ConfigError) -> eve_web_panel_api::PanelError {
    use eve_web_panel_api::PanelError;
    match error {
        ConfigError::RevisionConflict { .. } | ConfigError::StaleRequest => PanelError::Stale,
        ConfigError::UnknownNamespace(_) => PanelError::NotFound,
        ConfigError::Storage(_) | ConfigError::Unavailable => PanelError::Unavailable,
        _ => PanelError::InvalidInput,
    }
}
fn page_descriptor(namespace: &str) -> eve_web_panel_api::PageDescriptor {
    eve_web_panel_api::PageDescriptor {
        id: namespace.into(),
        title: match namespace {
            "runtime.llm" => "模型运行设置",
            "provider.openai" => "主模型连接",
            "provider.jev" => "Jev 连接",
            "runtime.models" => "模型角色与 Jev",
            "runtime.messages" => "消息路由设置",
            _ => namespace,
        }.into(),
        description: "保存后适用于新请求；标记需重启的字段在重启后生效。恢复默认会移除该字段的文件覆盖，继续使用环境配置或 Schema 默认值。".into(),
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConfig {
    format_version: u32,
    current: ConfigDocument,
    backups: Vec<ConfigBackup>,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            current: ConfigDocument::default(),
            backups: Vec::new(),
        }
    }
}

struct State {
    lock_file: Option<DirectoryLock>,
    stored: StoredConfig,
    live: Snapshots,
    initial: Snapshots,
    immediate: Snapshots,
    immediate_revision: u64,
}

struct DirectoryLock(File);

impl Drop for DirectoryLock {
    fn drop(&mut self) {
        // 覆盖恢复失败及最终析构路径，避免继承的描述符延长目录锁寿命。
        let _ = self.0.unlock();
    }
}

pub(crate) struct FileConfigService {
    directory: PathBuf,
    schemas: BTreeMap<String, ConfigSchema>,
    environment: BTreeMap<String, String>,
    session: String,
    validator: Option<std::sync::Arc<dyn ConfigValidator>>,
    state: Mutex<State>,
}

impl FileConfigService {
    pub(crate) fn web_pages(
        &self,
    ) -> eve_web_panel_api::PanelResult<Vec<eve_web_panel_api::PageDescriptor>> {
        let _state = self.state().map_err(panel_error)?;
        if self.schemas.len() > eve_web_panel_api::MAX_PLUGIN_PAGES {
            return Err(eve_web_panel_api::PanelError::Unavailable);
        }
        Ok(self
            .schemas
            .keys()
            .map(|namespace| page_descriptor(namespace))
            .collect())
    }
    pub(crate) fn web_read(
        &self,
        namespace: &str,
    ) -> eve_web_panel_api::PanelResult<eve_web_panel_api::PluginPage> {
        use eve_web_panel_api::*;
        let state = self.state().map_err(panel_error)?;
        let schema = self.schemas.get(namespace).ok_or(PanelError::NotFound)?;
        let overrides = state.stored.current.namespaces.get(namespace);
        let snapshot = select(&state.live, namespace, schema.version).map_err(panel_error)?;
        let fields = schema
            .fields
            .iter()
            .map(|(id, field)| {
                let override_value = overrides.and_then(|entry| entry.values.get(id)).cloned();
                let source = if override_value.is_some() {
                    "override"
                } else if field
                    .environment
                    .as_ref()
                    .is_some_and(|key| self.environment.contains_key(key))
                {
                    "environment"
                } else {
                    "default"
                };
                PageField {
                    id: id.clone(),
                    label: id.clone(),
                    value: snapshot.values.get(id).cloned(),
                    override_value,
                    source,
                    restart_required: field.restart_required,
                    kind: match field.kind {
                        ConfigKind::Boolean => PageFieldKind::Boolean,
                        ConfigKind::String => PageFieldKind::Text,
                        ConfigKind::Integer { minimum, maximum } => {
                            PageFieldKind::Integer { minimum, maximum }
                        }
                    },
                }
            })
            .collect();
        let page = PluginPage {
            descriptor: page_descriptor(namespace),
            instance: self.session.clone(),
            revision: state.stored.current.revision,
            fields,
        };
        page.validate()?;
        Ok(page)
    }
    pub(crate) fn web_save(
        &self,
        request: &eve_web_panel_api::PageSaveRequest,
    ) -> eve_web_panel_api::PanelResult<eve_web_panel_api::PageSaved> {
        use eve_web_panel_api::*;
        if request.plugin_id != CONFIG_PLUGIN_ID
            || request.values.is_empty()
            || request.values.len() > MAX_PAGE_FIELDS
        {
            return Err(PanelError::InvalidInput);
        }
        let mut state = self.state().map_err(panel_error)?;
        if request.instance != self.session
            || request.expected_revision != state.stored.current.revision
        {
            return Err(PanelError::Stale);
        }
        let schema = self
            .schemas
            .get(&request.page_id)
            .ok_or(PanelError::NotFound)?;
        let mut overrides = state.stored.current.namespaces.clone();
        let entry = overrides
            .entry(schema.namespace.clone())
            .or_insert_with(|| NamespaceValues {
                schema_version: schema.version,
                values: BTreeMap::new(),
            });
        for (id, value) in &request.values {
            let field = schema.fields.get(id).ok_or(PanelError::InvalidInput)?;
            if field.sensitive || field.encrypted {
                return Err(PanelError::Forbidden);
            }
            if let Some(value) = value {
                if value.to_string().len() > 8192 {
                    return Err(PanelError::InvalidInput);
                }
                field
                    .validate_value(&format!("{}.{}", schema.namespace, id), value)
                    .map_err(panel_error)?;
                entry.values.insert(id.clone(), value.clone());
            } else {
                entry.values.remove(id);
            }
        }
        if entry.values.is_empty() {
            overrides.remove(&schema.namespace);
        }
        let change = self
            .replace_locked(
                &mut state,
                request.expected_revision,
                overrides,
                ApplyMode::NewRequests,
            )
            .map_err(panel_error)?;
        Ok(PageSaved {
            revision: change.revision,
            restart_required: change.restart_required,
        })
    }
    pub(crate) fn open(bootstrap: &ConfigBootstrap) -> ConfigResult<Self> {
        let mut schemas = BTreeMap::new();
        let mut environment_names = BTreeSet::new();
        for schema in &bootstrap.schemas {
            schema.validate()?;
            for (key, field) in &schema.fields {
                let path = format!("{}.{}", schema.namespace, key);
                if field.sensitive || field.encrypted {
                    return Err(ConfigError::UnsupportedSensitiveField(path));
                }
                if let Some(name) = &field.environment
                    && !environment_names.insert(name.clone())
                {
                    return Err(ConfigError::InvalidSchema(path));
                }
            }
            if schemas
                .insert(schema.namespace.clone(), schema.clone())
                .is_some()
            {
                return Err(ConfigError::InvalidSchema(schema.namespace.clone()));
            }
        }
        let environment = if let Some(values) = &bootstrap.environment {
            values.clone()
        } else {
            let mut values = BTreeMap::new();
            for name in environment_names {
                match std::env::var(&name) {
                    Ok(value) => {
                        values.insert(name, value);
                    }
                    Err(std::env::VarError::NotPresent) => {}
                    Err(std::env::VarError::NotUnicode(_)) => {
                        return Err(ConfigError::InvalidValue(name));
                    }
                }
            }
            values
        };
        fs::create_dir_all(&bootstrap.directory).map_err(|e| storage("创建目录", e))?;
        let directory = bootstrap
            .directory
            .canonicalize()
            .map_err(|e| storage("解析目录", e))?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("config.lock"))
            .map_err(|e| storage("打开目录锁", e))?;
        lock_file
            .try_lock()
            .map_err(|e| storage("目录被占用或无法加锁", e))?;
        let lock_file = DirectoryLock(lock_file);
        let stored: StoredConfig = read_stored(&directory.join("config.json"))?;
        if stored.format_version != FORMAT_VERSION {
            return Err(ConfigError::InvalidDocument("不支持的文件格式版本".into()));
        }
        validate_overrides(&schemas, &stored.current.namespaces)?;
        let mut previous = None;
        for backup in &stored.backups {
            if backup.document.revision >= stored.current.revision
                || previous.is_some_and(|revision| revision >= backup.document.revision)
            {
                return Err(ConfigError::InvalidDocument("备份修订顺序无效".into()));
            }
            previous = Some(backup.document.revision);
            validate_overrides(&schemas, &backup.document.namespaces)?;
        }
        if stored
            .backups
            .iter()
            .filter(|backup| !backup.pinned)
            .count()
            > DEFAULT_BACKUP_LIMIT
        {
            return Err(ConfigError::InvalidDocument(
                "自动备份数量超过保留上限".into(),
            ));
        }
        let live = resolve(&schemas, &environment, &stored.current)?;
        if let Some(validator) = &bootstrap.validator {
            validator.validate(&live)?;
        }
        let immediate_revision = stored.current.revision;
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| storage("创建会话标识", e))?
            .as_nanos();
        let session = format!(
            "{}-{time}-{}",
            std::process::id(),
            SESSION_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        Ok(Self {
            directory,
            schemas,
            environment,
            session,
            validator: bootstrap.validator.clone(),
            state: Mutex::new(State {
                lock_file: Some(lock_file),
                stored,
                initial: live.clone(),
                immediate: live.clone(),
                live,
                immediate_revision,
            }),
        })
    }

    fn state(&self) -> ConfigResult<MutexGuard<'_, State>> {
        let state = self.state.lock().map_err(|_| ConfigError::Unavailable)?;
        if state.lock_file.is_none() {
            return Err(ConfigError::Unavailable);
        }
        Ok(state)
    }

    pub(crate) fn close(&self) -> ConfigResult<()> {
        let mut state = self.state.lock().map_err(|_| ConfigError::Unavailable)?;
        if let Some(file) = &state.lock_file {
            file.0.unlock().map_err(|e| storage("释放目录锁", e))?;
        }
        state.lock_file = None;
        Ok(())
    }

    fn commit(&self, stored: &StoredConfig) -> ConfigResult<()> {
        let bytes = serde_json::to_vec_pretty(stored).map_err(|e| storage("编码配置", e))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".config-")
            .tempfile_in(&self.directory)
            .map_err(|e| storage("创建临时文件", e))?;
        temporary
            .write_all(&bytes)
            .map_err(|e| storage("写入临时文件", e))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| storage("同步临时文件", e))?;
        temporary
            .persist(self.directory.join("config.json"))
            .map_err(|e| storage("替换配置文件", e.error))?;
        Ok(())
    }

    fn replace_locked(
        &self,
        state: &mut State,
        expected_revision: u64,
        overrides: ConfigOverrides,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange> {
        if expected_revision != state.stored.current.revision {
            return Err(ConfigError::RevisionConflict {
                expected: expected_revision,
                found: state.stored.current.revision,
            });
        }
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| ConfigError::InvalidDocument("修订号已耗尽".into()))?;
        validate_overrides(&self.schemas, &overrides)?;
        let document = ConfigDocument {
            revision,
            namespaces: overrides,
        };
        let mut live = resolve(&self.schemas, &self.environment, &document)?;
        if let Some(validator) = &self.validator {
            validator.validate(&live)?;
        }
        let mut restart_required = Vec::new();
        for (namespace, schema) in &self.schemas {
            let values = &mut live.get_mut(namespace).expect("resolved schema").values;
            let initial = &state.initial[namespace].values;
            for (key, field) in &schema.fields {
                if field.restart_required {
                    if values.get(key) != initial.get(key) {
                        restart_required.push(format!("{namespace}.{key}"));
                    }
                    if let Some(value) = initial.get(key) {
                        values.insert(key.clone(), value.clone());
                    } else {
                        values.remove(key);
                    }
                }
            }
        }
        let mut candidate = state.stored.clone();
        candidate.backups.push(ConfigBackup {
            document: candidate.current.clone(),
            pinned: false,
        });
        prune(&mut candidate.backups);
        candidate.current = document;
        // 文件和备份同属一次提交；失败之前不改变 live 或 stored。
        self.commit(&candidate)?;
        state.stored = candidate;
        state.live = live;
        if mode == ApplyMode::Immediate {
            state.immediate = state.live.clone();
            state.immediate_revision = revision;
        }
        Ok(ConfigChange {
            revision,
            restart_required,
        })
    }
}

impl ConfigService for FileConfigService {
    fn snapshot(&self, namespace: &str, schema_version: u32) -> ConfigResult<ConfigSnapshot> {
        select(&self.state()?.live, namespace, schema_version)
    }
    fn begin_request(&self, namespace: &str, schema_version: u32) -> ConfigResult<ConfigRequest> {
        let state = self.state()?;
        Ok(ConfigRequest::new(
            self.session.clone(),
            select(&state.live, namespace, schema_version)?,
            state.immediate_revision,
        ))
    }
    fn read_request(&self, request: &ConfigRequest) -> ConfigResult<ConfigSnapshot> {
        let state = self.state()?;
        if request.session() != self.session {
            return Err(ConfigError::StaleRequest);
        }
        let initial = request.initial();
        if request.immediate_revision() < state.immediate_revision {
            select(&state.immediate, &initial.namespace, initial.schema_version)
        } else {
            // 校验 namespace/version，但不将之后的 NewRequests 更新带入当前请求。
            select(&state.live, &initial.namespace, initial.schema_version)?;
            Ok(initial.clone())
        }
    }
}

impl ConfigAdmin for FileConfigService {
    fn current(&self) -> ConfigResult<ConfigDocument> {
        Ok(self.state()?.stored.current.clone())
    }
    fn replace(
        &self,
        expected_revision: u64,
        overrides: ConfigOverrides,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange> {
        let mut state = self.state()?;
        self.replace_locked(&mut state, expected_revision, overrides, mode)
    }
    fn backups(&self) -> ConfigResult<Vec<ConfigBackup>> {
        Ok(self.state()?.stored.backups.clone())
    }
    fn pin_backup(&self, revision: u64, pinned: bool) -> ConfigResult<()> {
        let mut state = self.state()?;
        let mut candidate = state.stored.clone();
        let backup = candidate
            .backups
            .iter_mut()
            .find(|b| b.document.revision == revision)
            .ok_or(ConfigError::BackupNotFound(revision))?;
        backup.pinned = pinned;
        prune(&mut candidate.backups);
        self.commit(&candidate)?;
        state.stored = candidate;
        Ok(())
    }
    fn rollback(
        &self,
        expected_revision: u64,
        revision: u64,
        mode: ApplyMode,
    ) -> ConfigResult<ConfigChange> {
        let mut state = self.state()?;
        let overrides = state
            .stored
            .backups
            .iter()
            .find(|b| b.document.revision == revision)
            .ok_or(ConfigError::BackupNotFound(revision))?
            .document
            .namespaces
            .clone();
        self.replace_locked(&mut state, expected_revision, overrides, mode)
    }
}

fn select(
    snapshots: &Snapshots,
    namespace: &str,
    schema_version: u32,
) -> ConfigResult<ConfigSnapshot> {
    let snapshot = snapshots
        .get(namespace)
        .ok_or_else(|| ConfigError::UnknownNamespace(namespace.into()))?;
    if snapshot.schema_version != schema_version {
        return Err(ConfigError::SchemaVersion {
            namespace: namespace.into(),
            expected: snapshot.schema_version,
            found: schema_version,
        });
    }
    Ok(snapshot.clone())
}

fn validate_overrides(
    schemas: &BTreeMap<String, ConfigSchema>,
    overrides: &ConfigOverrides,
) -> ConfigResult<()> {
    for (namespace, values) in overrides {
        let schema = schemas
            .get(namespace)
            .ok_or_else(|| ConfigError::UnknownNamespace(namespace.clone()))?;
        if values.schema_version != schema.version {
            return Err(ConfigError::SchemaVersion {
                namespace: namespace.clone(),
                expected: schema.version,
                found: values.schema_version,
            });
        }
        for (key, value) in &values.values {
            let path = format!("{namespace}.{key}");
            schema
                .fields
                .get(key)
                .ok_or_else(|| ConfigError::UnknownField(path.clone()))?
                .validate_value(&path, value)?;
        }
    }
    Ok(())
}

fn resolve(
    schemas: &BTreeMap<String, ConfigSchema>,
    environment: &BTreeMap<String, String>,
    document: &ConfigDocument,
) -> ConfigResult<Snapshots> {
    let mut snapshots = BTreeMap::new();
    for (namespace, schema) in schemas {
        let mut values = BTreeMap::new();
        for (key, field) in &schema.fields {
            let path = format!("{namespace}.{key}");
            let file = document
                .namespaces
                .get(namespace)
                .and_then(|entry| entry.values.get(key));
            let env = field
                .environment
                .as_ref()
                .and_then(|name| environment.get(name));
            let value = if let Some(value) = file {
                Some(value.clone())
            } else if let Some(raw) = env {
                Some(parse_environment(field, raw, &path)?)
            } else {
                field.default.clone()
            };
            if let Some(value) = value {
                field.validate_value(&path, &value)?;
                values.insert(key.clone(), value);
            } else if field.required {
                return Err(ConfigError::MissingValue(path));
            }
        }
        snapshots.insert(
            namespace.clone(),
            ConfigSnapshot {
                namespace: namespace.clone(),
                schema_version: schema.version,
                revision: document.revision,
                values,
            },
        );
    }
    Ok(snapshots)
}

fn parse_environment(field: &ConfigField, raw: &str, path: &str) -> ConfigResult<Value> {
    match field.kind {
        ConfigKind::String => Ok(Value::String(raw.into())),
        ConfigKind::Boolean => raw
            .parse::<bool>()
            .map(Value::Bool)
            .map_err(|_| ConfigError::InvalidValue(path.into())),
        ConfigKind::Integer { .. } => raw
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| ConfigError::InvalidValue(path.into())),
    }
}

fn prune(backups: &mut Vec<ConfigBackup>) {
    while backups.iter().filter(|backup| !backup.pinned).count() > DEFAULT_BACKUP_LIMIT {
        let oldest = backups
            .iter()
            .position(|backup| !backup.pinned)
            .expect("unpinned count");
        backups.remove(oldest);
    }
}

fn read_stored(path: &Path) -> ConfigResult<StoredConfig> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(ConfigError::InvalidDocument(
                    "配置必须为普通文件，不能为链接或目录".into(),
                ));
            }
            let bytes = fs::read(path).map_err(|e| storage("读取配置", e))?;
            // 不在错误里回显文件值。
            serde_json::from_slice(&bytes).map_err(|e| {
                ConfigError::InvalidDocument(format!(
                    "JSON 解析失败（行 {}，列 {}）",
                    e.line(),
                    e.column()
                ))
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoredConfig::default()),
        Err(e) => Err(storage("检查配置文件", e)),
    }
}

fn storage(operation: &str, error: impl std::fmt::Display) -> ConfigError {
    ConfigError::Storage(format!("{operation}：{error}"))
}

#[cfg(test)]
mod tests;
