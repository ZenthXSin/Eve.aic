use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

fn now_ms() -> Result<u64, ToolExecutionError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failed("系统时钟无效"))?
        .as_millis() as u64)
}

pub(super) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, ToolExecutionError> {
    read_bounded_scoped(path, limit, None)
}

fn read_stream(
    reader: &mut impl std::io::Read,
    limit: usize,
    context: Option<&ToolExecutionContext>,
) -> Result<Vec<u8>, ToolExecutionError> {
    use std::io::Read;
    let mut reader = reader.take(
        limit
            .checked_add(1)
            .ok_or_else(|| failed("升级读取上限无效"))? as u64,
    );
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        if context.is_some_and(stopped) {
            return Err(ToolExecutionError::Cancelled("升级读取已取消或到期".into()));
        }
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(failed("升级文件读取失败")),
        };
        if count == 0 {
            break;
        }
        let needed = bytes
            .len()
            .checked_add(count)
            .ok_or_else(|| failed("升级文件读取计数无效"))?;
        if needed > limit {
            return Err(failed("升级文件在读取期间超过上限"));
        }
        if needed > bytes.capacity() {
            let capacity = bytes.capacity().saturating_mul(2).max(needed).min(limit);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .map_err(|_| failed("升级文件读取内存不可用"))?;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if context.is_some_and(stopped) {
        return Err(ToolExecutionError::Cancelled("升级读取已取消或到期".into()));
    }
    Ok(bytes)
}

pub(super) fn read_bounded_scoped(
    path: &Path,
    limit: usize,
    context: Option<&ToolExecutionContext>,
) -> Result<Vec<u8>, ToolExecutionError> {
    if context.is_some_and(stopped) {
        return Err(ToolExecutionError::Cancelled("升级读取已取消或到期".into()));
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| failed("升级文件不存在或不可读取"))?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(failed("升级文件必须为大小受限的普通文件"));
    }
    let mut file = fs::File::open(path).map_err(|_| failed("升级文件不可读取"))?;
    if !file
        .metadata()
        .map_err(|_| failed("升级文件不可核对"))?
        .is_file()
    {
        return Err(failed("升级文件类型已改变"));
    }
    read_stream(&mut file, limit, context)
}

fn digest_stream(
    reader: &mut impl std::io::Read,
    limit: u64,
    context: &ToolExecutionContext,
) -> Result<(String, u64), ToolExecutionError> {
    use std::io::Read;
    let mut reader = reader.take(
        limit
            .checked_add(1)
            .ok_or_else(|| failed("候选摘要上限无效"))?,
    );
    let mut size = 0u64;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 65536];
    loop {
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled("升级校验已取消或到期".into()));
        }
        let count = match reader.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(failed("candidate_changed：候选读取失败")),
        };
        if count == 0 {
            break;
        }
        size += count as u64;
        if size > limit {
            return Err(failed("candidate_changed：候选读取期间超过上限"));
        }
        digest.update(&buffer[..count]);
    }
    if stopped(context) {
        return Err(ToolExecutionError::Cancelled("升级校验已取消或到期".into()));
    }
    Ok((
        digest
            .finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        size,
    ))
}

fn file_digest(
    path: &Path,
    limit: u64,
    context: &ToolExecutionContext,
) -> Result<String, ToolExecutionError> {
    if stopped(context) {
        return Err(ToolExecutionError::Cancelled("升级校验已取消或到期".into()));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| failed("candidate_missing：候选文件不存在"))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(failed("candidate_changed：候选文件类型或大小改变"));
    }
    let mut file = fs::File::open(path).map_err(|_| failed("candidate_missing：候选文件不可读"))?;
    if !file
        .metadata()
        .map_err(|_| failed("candidate_changed：候选不可核对"))?
        .is_file()
    {
        return Err(failed("candidate_changed：候选文件类型改变"));
    }
    let (hash, size) = digest_stream(&mut file, limit, context)?;
    if size != metadata.len() {
        return Err(failed("candidate_changed：候选读取期间大小改变"));
    }
    Ok(hash)
}

fn read_manifest(directory: &Path) -> Result<(Vec<u8>, Value), ToolExecutionError> {
    let path = directory.join("manifest.json");
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| failed("candidate_missing：候选清单不存在"))?;
    if !metadata.is_file() || metadata.len() > LIVE_UPGRADE_MAX_JSON as u64 {
        return Err(failed("candidate_changed：候选清单类型或大小无效"));
    }
    let bytes = read_bounded(&path, LIVE_UPGRADE_MAX_JSON)
        .map_err(|_| failed("candidate_changed：候选清单不可读、类型无效或超过上限"))?;
    let manifest =
        serde_json::from_slice(&bytes).map_err(|_| failed("candidate_changed：候选清单损坏"))?;
    Ok((bytes, manifest))
}

fn verify_contents(
    directory: &Path,
    manifest: &Value,
    id: &str,
    context: &ToolExecutionContext,
) -> Result<(), ToolExecutionError> {
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
        || file_digest(&binary, 2 * 1024 * 1024 * 1024, context)?
            != manifest["binary_sha256"].as_str().unwrap_or("")
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
        if total > 64 * 1024 * 1024
            || file_digest(&path, MAX_FILE as u64, context)? != expected.as_str().unwrap_or("")
        {
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
            if stopped(context) {
                return Err(ToolExecutionError::Cancelled("冻结资源已取消或到期".into()));
            }
            let data =
                read_bounded_scoped(&path, MAX_FILE, Some(context)).map_err(
                    |error| match error {
                        error @ ToolExecutionError::Cancelled(_) => error,
                        _ => failed("运行资源不可读、类型无效或超过8MiB"),
                    },
                )?;
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
            verify_contents(&directory, &old, &candidate_id, context)?;
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
        let bytes = read_bounded(&directory.join("supervisor.json"), LIVE_UPGRADE_MAX_JSON)
            .map_err(|_| failed("实时升级监督器状态不可读、类型无效或超过上限"))?;
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
        verify_contents(&candidate, &manifest, &a.candidate_sha256, context)?;
        let lease_path = self
            .journal
            .join("leases")
            .join(format!("{}.json", a.candidate_sha256));
        if !lease_path.exists() {
            return Err(failed("prepare_lease_missing：请先准备候选"));
        }
        let lease_bytes = read_bounded(&lease_path, LIVE_UPGRADE_MAX_JSON)
            .map_err(|_| failed("prepare_lease_changed：准备租约不可读、类型无效或超过上限"))?;
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
                &read_bounded(&target, LIVE_UPGRADE_MAX_JSON)
                    .map_err(|_| failed("待处理升级请求不可读、类型无效或超过上限；保留原文件"))?,
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

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::io::{self, Read};

    struct Growing {
        read: u64,
    }
    impl Read for Growing {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            output.fill(b'x');
            self.read += output.len() as u64;
            Ok(output.len())
        }
    }
    #[test]
    fn digest_scan_bounds_a_growing_stream_and_retries_interrupted_reads() {
        let ctx = ToolExecutionContext::new();
        let mut growing = Growing { read: 0 };
        assert!(digest_stream(&mut growing, 7, &ctx).is_err());
        assert_eq!(growing.read, 8, "不可继续追读不断增长的候选");
        let mut growing = Growing { read: 0 };
        assert!(read_stream(&mut growing, 7, None).is_err());
        assert_eq!(growing.read, 8, "正文缓冲也必须在上限处停止");
        assert_eq!(
            read_stream(&mut &b"fixture"[..], 7, Some(&ctx)).unwrap(),
            b"fixture"
        );
        struct Interrupted {
            first: bool,
            remaining: &'static [u8],
        }
        impl Read for Interrupted {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                std::io::Read::read(&mut self.remaining, output)
            }
        }
        let mut source = Interrupted {
            first: true,
            remaining: b"fixture",
        };
        assert_eq!(
            digest_stream(&mut source, 7, &ctx).unwrap(),
            (hash(b"fixture"), 7)
        );
        assert_eq!(
            digest_stream(&mut &b""[..], 0, &ctx).unwrap(),
            (hash(b""), 0)
        );
    }
    #[test]
    fn digest_cancellation_before_read_and_at_eof_never_returns_success() {
        let ctx = ToolExecutionContext::new();
        ctx.cancellation_handle().cancel();
        let mut source = Growing { read: 0 };
        assert!(matches!(
            digest_stream(&mut source, 7, &ctx),
            Err(ToolExecutionError::Cancelled(_))
        ));
        assert_eq!(source.read, 0);
        let ctx = ToolExecutionContext::new();
        struct CancelAtEof(ToolExecutionContext);
        impl Read for CancelAtEof {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.0.cancellation_handle().cancel();
                Ok(0)
            }
        }
        assert!(matches!(
            digest_stream(&mut CancelAtEof(ctx.clone()), 7, &ctx),
            Err(ToolExecutionError::Cancelled(_))
        ));
        let ctx = ToolExecutionContext::new();
        assert!(matches!(
            read_stream(&mut CancelAtEof(ctx.clone()), 7, Some(&ctx)),
            Err(ToolExecutionError::Cancelled(_))
        ));
        let expired = ToolExecutionContext::new().with_deadline(Some(std::time::Instant::now()));
        assert!(matches!(
            digest_stream(&mut &b"fixture"[..], 7, &expired),
            Err(ToolExecutionError::Cancelled(_))
        ));
    }
    #[test]
    fn bounded_upgrade_files_reject_oversize_and_preserve_original_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest.json");
        let original = b"{\"fixed\":true}";
        fs::write(&path, original).unwrap();
        assert_eq!(read_bounded(&path, original.len()).unwrap(), original);
        assert!(read_bounded(&path, original.len() - 1).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            file_digest(&path, original.len() as u64, &ToolExecutionContext::new()).unwrap(),
            hash(original)
        );
        assert!(file_digest(&path, 1, &ToolExecutionContext::new()).is_err());
        assert!(read_bounded(directory.path(), 100).is_err());
        #[cfg(unix)]
        {
            let link = directory.path().join("link.json");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(read_bounded(&link, 100).is_err());
            assert!(file_digest(&link, 100, &ToolExecutionContext::new()).is_err());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }
}
