use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> Result<u64, ToolExecutionError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failed("系统时钟无效"))?
        .as_millis() as u64)
}

fn file_digest(path: &Path) -> Result<String, ToolExecutionError> {
    use std::io::Read;
    let metadata =
        fs::symlink_metadata(path).map_err(|_| failed("candidate_missing：候选文件不存在"))?;
    if !metadata.is_file() || metadata.len() > 2 * 1024 * 1024 * 1024 {
        return Err(failed("candidate_changed：候选文件类型或大小改变"));
    }
    let mut file = fs::File::open(path).map_err(|_| failed("candidate_missing：候选文件不可读"))?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 65536];
    loop {
        let size = file
            .read(&mut buffer)
            .map_err(|_| failed("candidate_changed：候选读取失败"))?;
        if size == 0 {
            break;
        }
        context.update(&buffer[..size]);
    }
    Ok(context
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn read_manifest(directory: &Path) -> Result<(Vec<u8>, Value), ToolExecutionError> {
    let path = directory.join("manifest.json");
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| failed("candidate_missing：候选清单不存在"))?;
    if !metadata.is_file() || metadata.len() > LIVE_UPGRADE_MAX_JSON as u64 {
        return Err(failed("candidate_changed：候选清单类型或大小无效"));
    }
    let bytes = fs::read(path).map_err(|_| failed("candidate_missing：候选清单不可读"))?;
    let manifest =
        serde_json::from_slice(&bytes).map_err(|_| failed("candidate_changed：候选清单损坏"))?;
    Ok((bytes, manifest))
}

fn verify_contents(directory: &Path, manifest: &Value, id: &str) -> Result<(), ToolExecutionError> {
    if !fs::symlink_metadata(directory)
        .map_err(|_| failed("candidate_missing：候选不存在"))?
        .is_dir()
    {
        return Err(failed("candidate_changed：候选目录类型无效"));
    }
    if manifest["format_version"] != 1
        || manifest["prepared"] != true
        || manifest["success"] != true
        || manifest["candidate_sha256"] != id
        || manifest["checks"]
            .as_array()
            .is_none_or(|checks| checks.len() != 4 || checks.iter().any(|c| c["success"] != true))
    {
        return Err(failed("candidate_changed：缺少完整硬性验证证据"));
    }
    let binary = directory.join(if cfg!(windows) {
        "eve-qqbot.exe"
    } else {
        "eve-qqbot"
    });
    if manifest["candidate_path"] != binary.to_string_lossy().as_ref()
        || file_digest(&binary)? != manifest["binary_sha256"].as_str().unwrap_or("")
    {
        return Err(failed("candidate_changed：候选程序改变"));
    }
    let resources = manifest["resources"]
        .as_object()
        .ok_or_else(|| failed("candidate_changed：资源清单无效"))?;
    if resources.len() > 10000
        || [
            "AGENT.md",
            "connectors/qqbot/bridge.mjs",
            "connectors/qqbot/bridge-core.mjs",
            "connectors/qqbot/package.json",
            "connectors/qqbot/package-lock.json",
        ]
        .iter()
        .any(|name| !resources.contains_key(*name))
    {
        return Err(failed("candidate_changed：运行资源缺失"));
    }
    let mut total = 0u64;
    for (name, expected) in resources {
        if !(name == "AGENT.md"
            || name.starts_with("connectors/qqbot/")
            || name.starts_with("vendor/"))
            || name.contains('\\')
            || name.contains("//")
            || Path::new(name)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(failed("candidate_changed：资源路径无效"));
        }
        let mut path = directory.join("resources");
        for part in Path::new(name).components() {
            path.push(part);
            if fs::symlink_metadata(&path)
                .map_err(|_| failed("candidate_missing：运行资源不存在"))?
                .file_type()
                .is_symlink()
            {
                return Err(failed("candidate_changed：运行资源不能是链接"));
            }
        }
        total += fs::metadata(&path)
            .map_err(|_| failed("candidate_missing：资源不存在"))?
            .len();
        if total > 64 * 1024 * 1024 || file_digest(&path)? != expected.as_str().unwrap_or("") {
            return Err(failed("candidate_changed：资源内容改变"));
        }
    }
    if hash(
        json!({"source":manifest["source"], "binary_sha256":manifest["binary_sha256"],
        "resources":manifest["resources"]})
        .to_string()
        .as_bytes(),
    ) != id
    {
        return Err(failed("candidate_changed：内容地址不一致"));
    }
    Ok(())
}

fn upgrade_plan(manifest: &Value) -> Value {
    json!({"operation":"switch_binary_preserve_state", "candidate_sha256":manifest["candidate_sha256"],
        "state_schema_version":1, "validation_policy":"format-clippy-tests-release-build-v1"})
}

impl LocalDeveloper {
    pub(super) fn issue_upgrade_lease(
        &self,
        directory: &Path,
        mut manifest: Value,
    ) -> Result<Value, ToolExecutionError> {
        let (raw, _) = read_manifest(directory)?;
        let now = now_ms()?;
        let lease = UpgradeLease {
            format_version: 1,
            candidate_sha256: manifest["candidate_sha256"].as_str().unwrap().into(),
            manifest_sha256: hash(&raw),
            plan_sha256: hash(upgrade_plan(&manifest).to_string().as_bytes()),
            validation_sha256: hash(manifest["checks"].to_string().as_bytes()),
            state_schema_version: 1,
            created_at_ms: now,
            expires_at_ms: now + LIVE_UPGRADE_LEASE_MS,
        };
        let leases = self.journal.join("leases");
        fs::create_dir_all(&leases).map_err(|_| failed("准备租约目录不可创建"))?;
        let mut file =
            tempfile::NamedTempFile::new_in(&leases).map_err(|_| failed("准备租约暂存失败"))?;
        file.write_all(&serde_json::to_vec(&lease).map_err(|_| failed("准备租约编码失败"))?)
            .and_then(|_| file.as_file().sync_all())
            .map_err(|_| failed("准备租约保存失败"))?;
        file.persist(leases.join(format!("{}.json", lease.candidate_sha256)))
            .map_err(|_| failed("准备租约提交失败"))?;
        manifest["lease"] = serde_json::to_value(lease).map_err(|_| failed("准备租约编码失败"))?;
        Ok(manifest)
    }

    pub(super) async fn publish_upgrade(
        &self,
        source: Value,
        bytes: Vec<u8>,
        checks: Vec<Value>,
        context: &ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        let parent = self.journal.join("candidates");
        fs::create_dir_all(&parent).map_err(|_| failed("候选目录不可创建"))?;
        let stage = tempfile::tempdir_in(&parent).map_err(|_| failed("候选暂存失败"))?;
        let listing = self
            .command_limited(
                CommandArguments {
                    owner_confirmed: false,
                    program: "git".into(),
                    args: [
                        "ls-files",
                        "-z",
                        "--",
                        "AGENT.md",
                        "connectors/qqbot",
                        "vendor",
                    ]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                    cwd: ".".into(),
                    timeout_ms: Some(30_000),
                },
                context,
                2 * 1024 * 1024,
            )
            .await?;
        if listing["success"] != true || listing["stdout_truncated"] == true {
            return Err(failed("运行资源清单无法完整读取"));
        }
        let mut resources = BTreeMap::new();
        let mut total = 0usize;
        for name in listing["stdout"]
            .as_str()
            .ok_or_else(|| failed("资源清单缺失"))?
            .split('\0')
            .filter(|s| !s.is_empty())
        {
            let path = self.path(name, false)?;
            let metadata = fs::metadata(&path).map_err(|_| failed("运行资源不可读取"))?;
            if !metadata.is_file() || metadata.len() > MAX_FILE as u64 || resources.len() >= 10_000
            {
                return Err(failed("运行资源类型或大小不支持"));
            }
            let data = fs::read(&path).map_err(|_| failed("运行资源不可读取"))?;
            total += data.len();
            if total > 64 * 1024 * 1024 {
                return Err(failed("运行资源超过 64 MiB"));
            }
            let target = stage.path().join("resources").join(name);
            fs::create_dir_all(target.parent().expect("resource has parent"))
                .map_err(|_| failed("运行资源目录不可创建"))?;
            fs::write(target, &data).map_err(|_| failed("运行资源保存失败"))?;
            resources.insert(name.to_owned(), hash(&data));
        }
        self.authorize(context)?;
        if self.source_fingerprint(context).await? != source {
            return Err(failed("冻结资源时源码发生变化；没有发布候选"));
        }
        let digest = hash(&bytes);
        let identity = json!({"source":source,"binary_sha256":digest,"resources":resources});
        let candidate_id = hash(identity.to_string().as_bytes());
        let directory = parent.join(&candidate_id);
        if !directory.exists()
            && fs::read_dir(&parent)
                .map_err(|_| failed("候选目录不可读取"))?
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry.path().is_dir()
                        && valid_candidate_sha(&entry.file_name().to_string_lossy())
                })
                .count()
                >= 8
        {
            return Err(failed(
                "candidate_storage_limit：最多保留八份候选，请检查并清理未使用的候选",
            ));
        }
        let filename = if cfg!(windows) {
            "eve-qqbot.exe"
        } else {
            "eve-qqbot"
        };
        let candidate = directory.join(filename);
        let staged_binary = stage.path().join(filename);
        fs::write(&staged_binary, bytes).map_err(|_| failed("候选文件保存失败"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&staged_binary, fs::Permissions::from_mode(0o700))
                .map_err(|_| failed("候选可执行权限设置失败"))?;
        }
        let manifest = json!({"format_version":1,"success":true,"prepared":true,"applied":false,
            "source":source,"candidate_sha256":candidate_id,"binary_sha256":digest,"candidate_path":candidate,
            "resources":resources,"checks":checks,
            "next":"按仓库约定提交分支/PR；已装配监督器时用 apply_self_upgrade 提交切换，并用 self_upgrade_status 核对真实结果。"});
        if manifest.to_string().len() > LIVE_UPGRADE_MAX_JSON {
            return Err(failed("候选清单超过上限，未发布候选"));
        }
        fs::write(stage.path().join("manifest.json"), manifest.to_string())
            .map_err(|_| failed("候选清单保存失败"))?;
        for name in [filename, "manifest.json"] {
            fs::File::open(stage.path().join(name))
                .and_then(|f| f.sync_all())
                .map_err(|_| failed("候选持久化失败"))?;
        }
        if directory.exists() {
            // 相同摘要的已发布目录不得覆盖；损坏时保留现场并拒绝。
            let (_, old) = read_manifest(&directory)?;
            verify_contents(&directory, &old, &candidate_id)?;
            return self.issue_upgrade_lease(&directory, old);
        }
        fs::rename(stage.path(), &directory).map_err(|_| failed("候选原子发布失败；保留原文件"))?;
        self.issue_upgrade_lease(&directory, manifest)
    }

    fn supervisor(&self) -> Result<(PathBuf, UpgradeSupervisor), ToolExecutionError> {
        let directory = self
            .config
            .live_upgrade
            .clone()
            .ok_or_else(|| failed("宿主没有启用实时升级监督器"))?;
        let bytes = fs::read(directory.join("supervisor.json"))
            .map_err(|_| failed("实时升级监督器不可用"))?;
        if bytes.len() > LIVE_UPGRADE_MAX_JSON {
            return Err(failed("监督器状态超过上限"));
        }
        let status: UpgradeSupervisor =
            serde_json::from_slice(&bytes).map_err(|_| failed("监督器状态无效；保留原文件"))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| failed("系统时钟无效"))?
            .as_millis() as u64;
        if status.format_version != LIVE_UPGRADE_VERSION
            || status.pid == 0
            || !valid_candidate_sha(&status.active_sha256)
            || status.heartbeat_ms > now.saturating_add(1000)
            || now.saturating_sub(status.heartbeat_ms) > 15_000
        {
            return Err(failed("实时升级监督器心跳无效或已过期"));
        }
        Ok((directory, status))
    }

    pub(super) fn upgrade_status(&self) -> Result<Value, ToolExecutionError> {
        let (_, status) = self.supervisor()?;
        serde_json::to_value(status).map_err(|_| failed("监督器状态无法读取"))
    }

    pub(super) fn apply_upgrade(
        &self,
        a: ApplyUpgradeArguments,
        context: &ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        if !self.config.self_upgrade
            || !self.config.commands
            || !valid_candidate_sha(&a.candidate_sha256)
        {
            return Err(failed("实时升级未启用或候选摘要无效"));
        }
        let (queue, status) = self.supervisor()?;
        if !matches!(status.phase.as_str(), "starting" | "running" | "waiting") {
            return Err(failed("监督器正在切换或需要检查，未提交新请求"));
        }
        let candidate = self.journal.join("candidates").join(&a.candidate_sha256);
        if fs::symlink_metadata(&candidate)
            .map_err(|_| failed("候选不存在"))?
            .file_type()
            .is_symlink()
        {
            return Err(failed("候选目录不能是链接"));
        }
        let (bytes, manifest) = read_manifest(&candidate)?;
        if manifest["format_version"] != 1
            || manifest["prepared"] != true
            || manifest["success"] != true
            || manifest["candidate_sha256"] != a.candidate_sha256
            || manifest["checks"].as_array().is_none_or(|checks| {
                checks.len() != 4 || checks.iter().any(|c| c["success"] != true)
            })
        {
            return Err(failed("候选没有完整验证证据，未提交切换"));
        }
        verify_contents(&candidate, &manifest, &a.candidate_sha256)?;
        let lease_path = self
            .journal
            .join("leases")
            .join(format!("{}.json", a.candidate_sha256));
        let lease_bytes =
            fs::read(&lease_path).map_err(|_| failed("prepare_lease_missing：请先准备候选"))?;
        if lease_bytes.len() > LIVE_UPGRADE_MAX_JSON {
            return Err(failed("prepare_lease_changed：准备租约超过上限"));
        }
        let lease: UpgradeLease = serde_json::from_slice(&lease_bytes)
            .map_err(|_| failed("prepare_lease_changed：准备租约无效"))?;
        let now = now_ms()?;
        if lease.format_version != 1
            || lease.state_schema_version != 1
            || lease.candidate_sha256 != a.candidate_sha256
            || lease.manifest_sha256 != hash(&bytes)
            || lease.plan_sha256 != hash(upgrade_plan(&manifest).to_string().as_bytes())
            || lease.validation_sha256 != hash(manifest["checks"].to_string().as_bytes())
            || lease.expires_at_ms <= lease.created_at_ms
            || lease.expires_at_ms - lease.created_at_ms > LIVE_UPGRADE_LEASE_MS
        {
            return Err(failed(
                "prepare_lease_changed：准备内容、计划、验证或状态格式发生变化",
            ));
        }
        if lease.created_at_ms > now.saturating_add(1000) || lease.expires_at_ms <= now {
            return Err(failed("prepare_lease_expired：请重新准备候选"));
        }
        if status.result.as_deref() == Some("applied")
            && status.candidate_sha256.as_deref() == Some(a.candidate_sha256.as_str())
            && manifest["binary_sha256"] == status.active_sha256
        {
            return Ok(
                json!({"queued":false,"applied":true,"already_active":true,"candidate_sha256":a.candidate_sha256}),
            );
        }
        let scope = context
            .scope()
            .ok_or_else(|| failed("升级请求缺少可信会话"))?;
        let request = UpgradeRequest {
            format_version: LIVE_UPGRADE_VERSION,
            candidate_sha256: a.candidate_sha256,
            candidate_directory: fs::canonicalize(candidate)
                .map_err(|_| failed("候选目录不可读取"))?,
            session_id: scope.session_id.clone(),
            requested_by: scope.user_id.clone(),
            turn_id: context
                .turn_id()
                .ok_or_else(|| failed("升级请求缺少可信持久轮次"))?,
            lease: Some(lease),
        };
        self.authorize(context)?;
        let target = queue.join("request.json");
        if target.exists() {
            let old: UpgradeRequest = serde_json::from_slice(
                &fs::read(&target).map_err(|_| failed("待处理升级请求不可读取"))?,
            )
            .map_err(|_| failed("待处理升级请求损坏；保留原文件"))?;
            if old.candidate_sha256 != request.candidate_sha256
                || old.session_id != request.session_id
                || old.turn_id != request.turn_id
            {
                return Err(failed("已有不同的升级请求，未覆盖"));
            }
        } else {
            let mut file =
                tempfile::NamedTempFile::new_in(&queue).map_err(|_| failed("升级请求暂存失败"))?;
            file.write_all(&serde_json::to_vec(&request).map_err(|_| failed("升级请求编码失败"))?)
                .and_then(|_| file.as_file().sync_all())
                .map_err(|_| failed("升级请求保存失败"))?;
            file.persist_noclobber(target)
                .map_err(|_| failed("升级请求提交结果需核对，禁止盲目重试"))?;
        }
        Ok(
            json!({"queued":true,"applied":false,"candidate_sha256":request.candidate_sha256,
            "next":"先结束本轮回复以便送达，不在当前轮轮询等待应用；之后监督器在空闲时切换，后续新轮用 self_upgrade_status 核对真实结果。"}),
        )
    }
}
