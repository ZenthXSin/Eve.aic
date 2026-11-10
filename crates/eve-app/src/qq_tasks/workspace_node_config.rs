//! 只读取可信宿主的成组配置；模型不能选择运行时，也不会自动下载或收集宿主动态库。
use super::workspace_node::{WorkspaceNodeAdapter, WorkspaceNodeTool};
use crate::{AppError, CoreBootstrap};
use eve_developer_api::DeveloperBackend;
use eve_llm_api::ToolExecutionContext;
use eve_script_runtime::{FrozenNodeRuntime, FrozenNodeTestRunner};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const KEYS: [&str; 4] = [
    "EVE_WORKSPACE_NODE_ROOTFS",
    "EVE_WORKSPACE_NODE_ROOTFS_SHA256",
    "EVE_WORKSPACE_NODE_BINARY",
    "EVE_WORKSPACE_NODE_BWRAP",
];

struct Config {
    rootfs: PathBuf,
    sha256: String,
    node: String,
    bwrap: PathBuf,
}

pub(crate) struct Prepared {
    runner: FrozenNodeTestRunner,
    pub(crate) protected_paths: BTreeSet<PathBuf>,
}

impl Config {
    /// 传入快照，测试不修改全局环境；错误不包含配置值或私有路径。
    fn parse(values: &BTreeMap<String, OsString>) -> Result<Option<Self>, AppError> {
        if KEYS.iter().all(|key| !values.contains_key(*key)) {
            return Ok(None);
        }
        if KEYS
            .iter()
            .any(|key| values.get(*key).is_none_or(|value| value.is_empty()))
        {
            return Err("工作区 Node 运行时需要同时配置 ROOTFS、ROOTFS_SHA256、BINARY 与 BWRAP；不允许缺项或空值".into());
        }
        let rootfs = PathBuf::from(&values[KEYS[0]]);
        let bwrap = PathBuf::from(&values[KEYS[3]]);
        if !rootfs.is_absolute() || !bwrap.is_absolute() {
            return Err("工作区 Node 的 ROOTFS 与 BWRAP 必须是宿主绝对路径".into());
        }
        let sha256 = values[KEYS[1]]
            .to_str()
            .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("工作区 Node 的 ROOTFS_SHA256 必须是可信发布包的 SHA256")?
            .to_ascii_lowercase();
        let node = values[KEYS[2]]
            .to_str()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 512
                    && !value.chars().any(char::is_control)
                    && !value.contains('\\')
                    && value
                        .split('/')
                        .all(|part| !matches!(part, "" | "." | ".."))
                    && Path::new(value)
                        .components()
                        .all(|part| matches!(part, Component::Normal(_)))
            })
            .ok_or("工作区 Node 的 BINARY 必须是包内普通相对路径")?
            .to_string();
        Ok(Some(Self {
            rootfs,
            sha256,
            node,
            bwrap,
        }))
    }

    fn prepare(self) -> Result<Prepared, AppError> {
        let rootfs = self
            .rootfs
            .canonicalize()
            .map_err(|_| "工作区 Node 发布包目录不可读取")?;
        if !rootfs.is_dir() {
            return Err("工作区 Node 发布包必须是目录".into());
        }
        let bwrap = self
            .bwrap
            .canonicalize()
            .map_err(|_| "工作区 Node 启动器不可读取")?;
        if !bwrap.is_file() {
            return Err("工作区 Node 启动器必须是普通可执行文件".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if bwrap
                .metadata()
                .map_err(|_| "工作区 Node 启动器元数据不可读取")?
                .permissions()
                .mode()
                & 0o100
                == 0
            {
                return Err("工作区 Node 启动器必须具有执行权限".into());
            }
        }
        let context = ToolExecutionContext::new()
            .with_deadline(Some(Instant::now() + Duration::from_secs(60)));
        let runtime = FrozenNodeRuntime::prepare(&rootfs, &self.node, &self.sha256, &context)
            .map_err(|_| "工作区 Node 发布包校验或私有冻结失败；未装配运行器")?;
        let mut protected_paths =
            BTreeSet::from([rootfs, bwrap.clone(), runtime.snapshot_path().to_path_buf()]);
        protected_paths.insert(
            std::env::current_exe()
                .and_then(std::fs::canonicalize)
                .map_err(|_| "无法确认宿主可执行文件的保护范围")?,
        );
        let runner = FrozenNodeTestRunner::new(runtime, bwrap)
            .map_err(|_| "工作区 Node 可信启动器校验失败；未装配运行器")?;
        Ok(Prepared {
            runner,
            protected_paths,
        })
    }
}

pub(crate) fn prepare_from_environment() -> Result<Option<Prepared>, AppError> {
    let values = KEYS
        .iter()
        .filter_map(|key| std::env::var_os(key).map(|value| ((*key).to_owned(), value)))
        .collect();
    Config::parse(&values)?.map(Config::prepare).transpose()
}

/// 工具与合同能力一次装配；没有显式配置时，两者均保持关闭。
pub(crate) fn attach(
    prepared: Option<Prepared>,
    backend: Option<Arc<dyn DeveloperBackend>>,
    bootstrap: &mut CoreBootstrap,
    late: &super::Late,
) -> Result<Option<Arc<WorkspaceNodeAdapter>>, AppError> {
    let Some(prepared) = prepared else {
        return Ok(None);
    };
    let backend = backend.ok_or("配置了工作区 Node 运行时，但未装配实际开发后端")?;
    let adapter = WorkspaceNodeAdapter::new(prepared.runner, backend);
    late.workspace_node
        .set(adapter.clone())
        .map_err(|_| "工作区 Node 合同能力重复装配")?;
    bootstrap
        .tools
        .push(Arc::new(WorkspaceNodeTool(adapter.clone())));
    Ok(Some(adapter))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> BTreeMap<String, OsString> {
        BTreeMap::from([
            (KEYS[0].into(), "/trusted/node-rootfs".into()),
            (KEYS[1].into(), "a".repeat(64).into()),
            (KEYS[2].into(), "runtime/node".into()),
            (KEYS[3].into(), "/trusted/bin/bwrap".into()),
        ])
    }

    #[test]
    fn absent_configuration_keeps_runtime_disabled_and_partial_groups_fail() {
        assert!(Config::parse(&BTreeMap::new()).unwrap().is_none());
        for key in KEYS {
            let mut missing = values();
            missing.remove(key);
            assert!(Config::parse(&missing).is_err());
            let mut empty = values();
            empty.insert(key.into(), OsString::new());
            assert!(Config::parse(&empty).is_err());
        }
    }

    #[test]
    fn configuration_validates_digest_and_paths_without_reading_them() {
        let parsed = Config::parse(&values()).unwrap().unwrap();
        assert_eq!(parsed.node, "runtime/node");
        assert_eq!(parsed.sha256, "a".repeat(64));
        for (key, invalid) in [
            (KEYS[0], "private/rootfs"),
            (KEYS[3], "bwrap"),
            (KEYS[1], "not-a-sha256"),
            (KEYS[2], "/runtime/node"),
            (KEYS[2], "../node"),
            (KEYS[2], "runtime/./node"),
            (KEYS[2], "runtime//node"),
            (KEYS[2], "runtime/node/"),
            (KEYS[2], "runtime\\node"),
            (KEYS[2], "runtime/\nnode"),
        ] {
            let mut bad = values();
            bad.insert(key.into(), invalid.into());
            assert!(Config::parse(&bad).is_err());
        }
        let mut upper = values();
        upper.insert(KEYS[1].into(), "A".repeat(64).into());
        assert_eq!(
            Config::parse(&upper).unwrap().unwrap().sha256,
            "a".repeat(64)
        );
    }

    #[test]
    fn configuration_errors_do_not_echo_private_values() {
        let mut bad = values();
        bad.insert(KEYS[0].into(), "sensitive-rootfs-value".into());
        let error = Config::parse(&bad).err().unwrap().to_string();
        assert!(!error.contains("sensitive-rootfs-value"));
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    #[test]
    fn prepared_runtime_protects_published_and_private_bytes_and_rejects_wrong_hash() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("runtime")).unwrap();
        let node = rootfs.join("runtime/node");
        std::fs::write(&node, b"fixture executable bytes; never launched").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bwrap = directory.path().join("bwrap");
        std::fs::write(&bwrap, b"fixture launcher bytes; never launched").unwrap();
        std::fs::set_permissions(&bwrap, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hash =
            FrozenNodeRuntime::bundle_sha256(&rootfs, "runtime/node", &ToolExecutionContext::new())
                .unwrap();
        let config = |sha256| Config {
            rootfs: rootfs.clone(),
            sha256,
            node: "runtime/node".into(),
            bwrap: bwrap.clone(),
        };
        assert!(config("0".repeat(64)).prepare().is_err());
        let prepared = config(hash).prepare().unwrap();
        assert!(
            prepared
                .protected_paths
                .contains(&rootfs.canonicalize().unwrap())
        );
        assert!(
            prepared
                .protected_paths
                .contains(&bwrap.canonicalize().unwrap())
        );
        assert!(
            prepared
                .protected_paths
                .contains(&std::env::current_exe().unwrap().canonicalize().unwrap())
        );
        let snapshot = prepared
            .protected_paths
            .iter()
            .find(|path| **path != rootfs.canonicalize().unwrap() && path.is_dir())
            .unwrap()
            .clone();
        assert_eq!(
            std::fs::read(snapshot.join("runtime/node")).unwrap(),
            std::fs::read(node).unwrap()
        );
        drop(prepared);
        assert!(!snapshot.exists());
    }

    #[test]
    fn absent_runtime_does_not_register_tool_or_contract() {
        let late = super::super::Late::default();
        let mut bootstrap = CoreBootstrap {
            tool_scope_policy: None,
            host_config: Default::default(),
            configured_foreground_steps: false,
            api_key: String::new(),
            context: None,
            tools: Vec::new(),
            foreground_excluded_tools: Default::default(),
            health: None,
            history_digests: None,
            history_context_planner: None,
            image_source: None,
            steering: None,
            review_source: None,
        };
        assert!(attach(None, None, &mut bootstrap, &late).unwrap().is_none());
        assert!(bootstrap.tools.is_empty());
        assert!(late.workspace_node.get().is_none());
    }
}
