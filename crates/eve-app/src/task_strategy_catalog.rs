//! 正式宿主的固定验收目录；只匹配已获授权的真实任务，不创建训练/留出任务。
use crate::AppError;
use eve_config_api::{CONFIG_SERVICE_ID, ConfigService, ConfigServiceHandle, ConfigSnapshot};
use eve_plugin_api::{ServiceId, ServiceRegistry};
use eve_task_api::*;
use eve_task_plugin::PairedTaskStrategyPolicy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fmt,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::UNIX_EPOCH,
};

const MAX_CATALOG_BYTES: usize = 256 * 1024;
const MAX_CASES: usize = 128;
const CONFIG_NAMESPACES: [&str; 7] = [
    "provider.openai",
    "runtime.models",
    "provider.jev",
    "runtime.tasks",
    "runtime.people",
    "runtime.model_uses",
    "runtime.llm",
];
pub(crate) type ProfilesReader = dyn Fn(&TaskOwner) -> TaskResult<Value> + Send + Sync;
/// 宿主提供实际装配清单，不能使用从聊天或目录文件读取的能力自述。
pub(crate) struct RuntimeInputs {
    pub agent_path: PathBuf,
    pub developer_config: Option<PathBuf>,
    pub admin_bindings: PathBuf,
    /// 在真实工具、角色和执行器设置就绪后写入；初始化前禁止匹配。
    pub runtime_manifest: Arc<OnceLock<Value>>,
    /// 启用角色时按身份读取当前完整列表及每项 revision；读取失败不能当作空列表。
    pub profiles: Option<Arc<ProfilesReader>>,
    /// 当前已加载可执行文件的 SHA256，宿主启动时计算，不是目录提供的版本字符串。
    pub executable_sha256: String,
}
#[derive(Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Train,
    Holdout,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Case {
    owner: TaskOwner,
    family: String,
    input_sha256: String,
    kind: Kind,
    contract: TaskEvaluationContract,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    format_version: u32,
    revision: u64,
    cases: Vec<Case>,
}
impl Catalog {
    fn parse(bytes: &[u8]) -> TaskResult<Self> {
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(TaskError::LimitReached);
        }
        let value = strict::from_slice(bytes).map_err(|_| TaskError::InvalidInput)?;
        let catalog: Self = serde_json::from_value(value).map_err(|_| TaskError::InvalidInput)?;
        if catalog.format_version != 1 {
            return Err(TaskError::UnsupportedVersion);
        }
        if catalog.revision == 0 || catalog.cases.len() > MAX_CASES {
            return Err(TaskError::InvalidInput);
        }
        let mut inputs = BTreeSet::new();
        for case in &catalog.cases {
            validate_owner(&case.owner)?;
            validate_id(&case.family)?;
            case.contract.validate()?;
            // 同一身份+输入只能归属一个来源/留出类别和一个任务族，不能用改名泄漏训练题。
            if !valid_strategy_digest(&case.input_sha256)
                || !inputs.insert(task_strategy_digest(&(&case.owner, &case.input_sha256)))
            {
                return Err(TaskError::InvalidInput);
            }
            let same = |other: &&Case| other.owner == case.owner && other.family == case.family;
            let holdouts = catalog
                .cases
                .iter()
                .filter(same)
                .filter(|c| c.kind == Kind::Holdout)
                .count();
            let training = catalog
                .cases
                .iter()
                .filter(same)
                .filter(|c| c.kind == Kind::Train)
                .count();
            if !(2..=64).contains(&holdouts) || training > 64 {
                return Err(TaskError::InvalidInput);
            }
        }
        Ok(catalog)
    }
}
struct CatalogPolicy {
    catalog: Catalog,
    path: PathBuf,
    content_sha256: String,
    registry: Arc<dyn ServiceRegistry>,
    inputs: RuntimeInputs,
}
impl fmt::Debug for CatalogPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CatalogTaskStrategyPolicy")
            .field("revision", &self.catalog.revision)
            .field("cases", &self.catalog.cases.len())
            .finish_non_exhaustive()
    }
}
pub(crate) fn load(
    path: &Path,
    registry: Arc<dyn ServiceRegistry>,
    inputs: RuntimeInputs,
) -> Result<Arc<dyn TaskStrategyPolicy>, AppError> {
    if !valid_strategy_digest(&inputs.executable_sha256) {
        return Err("任务策略执行器摘要无效".into());
    }
    let (bytes, _) = read_file(path, MAX_CATALOG_BYTES)?;
    let catalog =
        Catalog::parse(&bytes).map_err(|_| "任务策略目录格式、版本或训练/留出隔离无效")?;
    Ok(Arc::new(CatalogPolicy {
        catalog,
        path: path.to_path_buf(),
        content_sha256: bytes_digest(&bytes),
        registry,
        inputs,
    }))
}
fn bytes_digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// 限长普通文件，读后核对元信息；mtime/ctime/inode 让内容改回也隔离旧经验。
fn read_file(path: &Path, limit: usize) -> TaskResult<(Vec<u8>, Value)> {
    let mut file = File::open(path).map_err(|_| TaskError::Unavailable)?;
    let before = file.metadata().map_err(|_| TaskError::Unavailable)?;
    if !before.is_file() || before.len() > limit as u64 {
        return Err(TaskError::InvalidInput);
    }
    let stamp = |meta: &std::fs::Metadata| -> TaskResult<Value> {
        let modified = meta
            .modified()
            .map_err(|_| TaskError::Unavailable)?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| TaskError::InvalidInput)?;
        let value = json!({"length":meta.len(),"modified_seconds":modified.as_secs(),"modified_nanos":modified.subsec_nanos()});
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mut value = value;
            value["device"] = json!(meta.dev());
            value["inode"] = json!(meta.ino());
            value["ctime_seconds"] = json!(meta.ctime());
            value["ctime_nanos"] = json!(meta.ctime_nsec());
            Ok(value)
        }
        #[cfg(not(unix))]
        {
            Ok(value)
        }
    };
    let before = stamp(&before)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| TaskError::Unavailable)?;
    if bytes.len() > limit
        || stamp(&file.metadata().map_err(|_| TaskError::Unavailable)?)? != before
    {
        return Err(TaskError::Unavailable);
    }
    Ok((bytes, before))
}
fn file_fingerprint(path: &Path, optional: bool) -> TaskResult<Value> {
    if optional && !path.try_exists().map_err(|_| TaskError::Unavailable)? {
        return Ok(json!({"absent":true}));
    }
    let (bytes, stamp) = read_file(path, 4 * 1024 * 1024)?;
    Ok(
        json!({"sha256":bytes_digest(&bytes),"stamp":stamp,"path_sha256":task_strategy_digest(&path)}),
    )
}
impl CatalogPolicy {
    fn matching(&self, task: &Task) -> Option<&Case> {
        let input = task_strategy_input(task);
        self.catalog
            .cases
            .iter()
            .find(|case| case.owner == task.owner && case.input_sha256 == input)
    }
    fn settings(&self) -> TaskResult<Arc<dyn ConfigService>> {
        let entry = self
            .registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).map_err(|_| TaskError::Unavailable)?)
            .map_err(|_| TaskError::Unavailable)?
            .ok_or(TaskError::Unavailable)?;
        if entry.owner.as_str() != "eve.config" {
            return Err(TaskError::Unavailable);
        }
        Ok(entry
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| TaskError::Unavailable)?
            .0
            .clone())
    }
    fn runtime(&self, owner: &TaskOwner) -> TaskResult<String> {
        let (catalog_bytes, catalog_stamp) = read_file(&self.path, MAX_CATALOG_BYTES)?;
        if bytes_digest(&catalog_bytes) != self.content_sha256 {
            return Err(TaskError::InvalidTransition);
        }
        let manifest = self
            .inputs
            .runtime_manifest
            .get()
            .ok_or(TaskError::Unavailable)?;
        if !manifest.is_object()
            || serde_json::to_vec(manifest)
                .map_err(|_| TaskError::InvalidInput)?
                .len()
                > 1024 * 1024
        {
            return Err(TaskError::InvalidInput);
        }
        let settings = self.settings()?;
        for _ in 0..4 {
            let snapshots: Vec<ConfigSnapshot> = CONFIG_NAMESPACES
                .iter()
                .map(|namespace| {
                    settings
                        .snapshot(namespace, 1)
                        .map_err(|_| TaskError::Unavailable)
                })
                .collect::<TaskResult<_>>()?;
            let revision = snapshots[0].revision;
            if snapshots.iter().any(|s| s.revision != revision) {
                continue;
            }
            let model = crate::models::summary(settings.as_ref(), Some(crate::qq_tasks::MODEL_USE))
                .map_err(|_| TaskError::Unavailable)?;
            let agent = file_fingerprint(&self.inputs.agent_path, false)?;
            let developer = self
                .inputs
                .developer_config
                .as_ref()
                .map(|p| file_fingerprint(p, false))
                .transpose()?;
            let authorization = file_fingerprint(&self.inputs.admin_bindings, true)?;
            let profiles = self
                .inputs
                .profiles
                .as_ref()
                .map(|read| read(owner))
                .transpose()?;
            if serde_json::to_vec(&profiles)
                .map_err(|_| TaskError::InvalidInput)?
                .len()
                > 1024 * 1024
            {
                return Err(TaskError::LimitReached);
            }
            let after = settings
                .snapshot(CONFIG_NAMESPACES[0], 1)
                .map_err(|_| TaskError::Unavailable)?;
            if after.revision != revision || after != snapshots[0] {
                continue;
            }
            return Ok(task_strategy_digest(&json!({
                "catalog_revision":self.catalog.revision,"catalog_sha256":self.content_sha256,"catalog_stamp":catalog_stamp,
                "configuration": snapshots,"actual_model":{"model":model.model,"protocol":model.protocol,"max_output_tokens":model.max_output_tokens},
                "agent":agent,"developer":developer,"authorization":authorization,"roles":profiles,
                "runtime_manifest":manifest,"executable_sha256":self.inputs.executable_sha256,
            })));
        }
        Err(TaskError::Unavailable)
    }
}
struct FixedScope(TaskStrategyScope);
impl fmt::Debug for FixedScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FixedScope(<redacted>)")
    }
}
impl TaskStrategyEnvironment for FixedScope {
    fn scope(&self, task: &Task) -> TaskResult<Option<TaskStrategyScope>> {
        Ok((task.owner == self.0.owner).then(|| self.0.clone()))
    }
}
impl TaskStrategyPolicy for CatalogPolicy {
    fn scope(&self, task: &Task) -> TaskResult<Option<TaskStrategyScope>> {
        let Some(case) = self.matching(task) else {
            return Ok(None);
        };
        Ok(Some(TaskStrategyScope {
            owner: case.owner.clone(),
            family: case.family.clone(),
            runtime_sha256: self.runtime(&task.owner)?,
            evaluator_version: RECEIPT_EVALUATOR_VERSION.into(),
        }))
    }
    fn source_contract(&self, task: &Task) -> TaskResult<Option<TaskEvaluationContract>> {
        let Some(case) = self.matching(task).filter(|c| c.kind == Kind::Train) else {
            return Ok(None);
        };
        self.scope(task)?;
        Ok(Some(case.contract.clone()))
    }
    fn assign(
        &self,
        task: &Task,
        snapshot: &TaskStrategySnapshot,
    ) -> TaskResult<Option<TaskStrategyAssignment>> {
        let Some(scope) = self.scope(task)? else {
            return Ok(None);
        };
        let cases = self
            .catalog
            .cases
            .iter()
            .filter(|c| {
                c.owner == scope.owner && c.family == scope.family && c.kind == Kind::Holdout
            })
            .map(|c| TaskStrategyCase {
                scope: scope.clone(),
                input_sha256: c.input_sha256.clone(),
                contract: c.contract.clone(),
            })
            .collect();
        // 只为这次匹配捕获当前范围，不能将启动时指纹塞进永久静态目录。
        PairedTaskStrategyPolicy::new(Arc::new(FixedScope(scope)), vec![], cases)?
            .assign(task, snapshot)
    }
}

mod strict {
    use serde::{
        Deserialize, Deserializer,
        de::{self, MapAccess, SeqAccess, Visitor},
    };
    use serde_json::{Map, Number, Value};
    use std::fmt;

    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Value, serde_json::Error> {
        let mut parser = serde_json::Deserializer::from_slice(bytes);
        let value = StrictValue::deserialize(&mut parser)?.0;
        parser.end()?;
        Ok(value)
    }
    struct StrictValue(Value);
    impl<'de> Deserialize<'de> for StrictValue {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(StrictVisitor).map(Self)
        }
    }
    struct StrictVisitor;
    impl<'de> Visitor<'de> for StrictVisitor {
        type Value = Value;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("JSON without duplicate keys")
        }
        fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
            Ok(Value::Bool(value))
        }
        fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
            Ok(Value::Number(value.into()))
        }
        fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
            Ok(Value::Number(value.into()))
        }
        fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
            Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| E::custom("invalid number"))
        }
        fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
            Ok(Value::String(value.into()))
        }
        fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
            Ok(Value::String(value))
        }
        fn visit_none<E: de::Error>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut array = Vec::new();
            while let Some(value) = seq.next_element::<StrictValue>()? {
                array.push(value.0);
            }
            Ok(Value::Array(array))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut object = Map::new();
            while let Some(key) = map.next_key::<String>()? {
                if object.contains_key(&key) {
                    return Err(de::Error::custom("duplicate JSON key"));
                }
                object.insert(key, map.next_value::<StrictValue>()?.0);
            }
            Ok(Value::Object(object))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_config_api::{ApplyMode, ConfigAdmin, NamespaceValues};
    use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
    use eve_kernel::{Kernel, KernelServices};
    use eve_plugin_api::PluginId;
    use serde_json::json;
    use std::collections::BTreeMap;
    fn task(text: &str) -> Task {
        Task {
            id: "task-1".into(),
            owner: TaskOwner {
                session_id: "session-a".into(),
                user_id: "user-a".into(),
            },
            spec: TaskSpec {
                title: "fixture".into(),
                instructions: text.into(),
                max_steps: 4,
                context: None,
            },
            source: "host-fixture".into(),
            created_at_ms: 1,
            status: TaskStatus::Running,
            cancel_requested: false,
            started_at_ms: Some(1),
            finished_at_ms: None,
            usage: None,
            result: None,
            failure: None,
            notice: None,
            execution: Some(Default::default()),
        }
    }
    fn case(text: &str, kind: Kind) -> Case {
        let task = task(text);
        Case {
            owner: task.owner.clone(),
            family: "file-check".into(),
            input_sha256: task_strategy_input(&task),
            kind,
            contract: TaskEvaluationContract {
                require_final_batch: true,
                checks: vec![TaskReceiptCheck {
                    tool: "read_file".into(),
                    arguments: json!({"path":"result.txt","offset":0}),
                    output_pointer: "/sha256".into(),
                    expected: json!("a".repeat(64)),
                }],
            },
        }
    }
    fn catalog() -> Catalog {
        Catalog {
            format_version: 1,
            revision: 1,
            cases: vec![
                case("train", Kind::Train),
                case("a", Kind::Holdout),
                case("b", Kind::Holdout),
            ],
        }
    }
    #[test]
    fn strict_catalog_rejects_duplicates_unknown_versions_and_train_holdout_leakage() {
        assert!(Catalog::parse(br#"{"format_version":1,"revision":1,"cases":[]}"#).is_ok());
        assert!(
            Catalog::parse(br#"{"format_version":1,"revision":1,"revision":2,"cases":[]}"#)
                .is_err()
        );
        assert!(Catalog::parse(br#"{"format_version":2,"revision":1,"cases":[]}"#).is_err());
        assert!(
            Catalog::parse(br#"{"format_version":1,"revision":1,"cases":[],"owner":"model"}"#)
                .is_err()
        );
        let mut data = catalog();
        data.cases[1].input_sha256 = data.cases[0].input_sha256.clone();
        assert!(Catalog::parse(&serde_json::to_vec(&data).unwrap()).is_err());
        let mut data = catalog();
        data.cases.pop();
        assert!(Catalog::parse(&serde_json::to_vec(&data).unwrap()).is_err());
        assert!(Catalog::parse(&vec![b' '; MAX_CATALOG_BYTES + 1]).is_err());
    }
    #[tokio::test]
    async fn live_scope_tracks_configuration_authorization_and_roles_and_separates_owners() {
        let root = tempfile::tempdir().unwrap();
        let services = KernelServices::default();
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        let plugin = ConfigPlugin::new(
            ConfigBootstrap::new(
                root.path().join("config"),
                vec![
                    crate::config::openai_schema(),
                    eve_config_api::model_roles_schema(),
                    crate::qq_message_judge::provider_schema(),
                    crate::qq_tasks::schema(),
                    crate::qq_people::schema(),
                    crate::config::model_uses_schema(),
                    eve_config_api::runtime_llm_schema(),
                ],
            )
            .with_environment(BTreeMap::new()),
        )
        .unwrap();
        let admin = plugin.controller();
        kernel.register(Box::new(plugin)).unwrap();
        kernel
            .start(&PluginId::new(eve_config_api::CONFIG_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let agent = root.path().join("AGENT.md");
        std::fs::write(&agent, "fixed prompt").unwrap();
        let bindings = root.path().join("admin.json");
        let path = root.path().join("catalog.json");
        std::fs::write(&path, serde_json::to_vec(&catalog()).unwrap()).unwrap();
        let manifest = Arc::new(OnceLock::new());
        manifest
            .set(json!({"tools":[{"name":"read_file"}]}))
            .unwrap();
        let roles = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let read_roles = roles.clone();
        let policy = load(
            &path,
            registry,
            RuntimeInputs {
                agent_path: agent,
                developer_config: None,
                admin_bindings: bindings.clone(),
                runtime_manifest: manifest,
                profiles: Some(Arc::new(move |_| {
                    Ok(json!({"revision": read_roles.load(std::sync::atomic::Ordering::SeqCst)}))
                })),
                executable_sha256: "e".repeat(64),
            },
        )
        .unwrap();
        let source = task("train");
        let first = policy.scope(&source).unwrap().unwrap();
        assert!(policy.source_contract(&source).unwrap().is_some());
        let mut foreign = source.clone();
        foreign.owner.user_id = "other".into();
        assert!(policy.scope(&foreign).unwrap().is_none());
        let old = admin.current().unwrap();
        let mut overrides = old.namespaces;
        overrides.insert(
            "runtime.tasks".into(),
            NamespaceValues {
                schema_version: 1,
                values: BTreeMap::from([("default_steps".into(), json!(60))]),
            },
        );
        admin
            .replace(old.revision, overrides, ApplyMode::Immediate)
            .unwrap();
        let second = policy.scope(&source).unwrap().unwrap();
        assert_ne!(first.runtime_sha256, second.runtime_sha256);
        roles.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let third = policy.scope(&source).unwrap().unwrap();
        assert_ne!(second.runtime_sha256, third.runtime_sha256);
        std::fs::write(bindings, "bound identity").unwrap();
        let fourth = policy.scope(&source).unwrap().unwrap();
        assert_ne!(third.runtime_sha256, fourth.runtime_sha256);
        std::fs::write(path, br#"{"format_version":1,"revision":2,"cases":[]}"#).unwrap();
        assert!(policy.scope(&source).is_err());
        kernel.stop_all().await.unwrap();
    }
}
