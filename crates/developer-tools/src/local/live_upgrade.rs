use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

impl LocalDeveloper {
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
        fs::write(stage.path().join("manifest.json"), manifest.to_string())
            .map_err(|_| failed("候选清单保存失败"))?;
        for name in [filename, "manifest.json"] {
            fs::File::open(stage.path().join(name))
                .and_then(|f| f.sync_all())
                .map_err(|_| failed("候选持久化失败"))?;
        }
        if directory.exists() {
            // 相同摘要的已发布目录不得覆盖；损坏时保留现场并拒绝。
            let old: Value = serde_json::from_slice(
                &fs::read(directory.join("manifest.json"))
                    .map_err(|_| failed("已有候选不可读取"))?,
            )
            .map_err(|_| failed("已有候选损坏"))?;
            if old["candidate_sha256"] != candidate_id
                || hash(&fs::read(&candidate).map_err(|_| failed("已有候选损坏"))?) != digest
            {
                return Err(failed("已有候选不一致；保留原文件"));
            }
            for (name, expected) in &resources {
                let path = directory.join("resources").join(name);
                if fs::symlink_metadata(&path)
                    .map_err(|_| failed("已有资源损坏"))?
                    .file_type()
                    .is_symlink()
                    || hash(&fs::read(&path).map_err(|_| failed("已有资源损坏"))?) != *expected
                {
                    return Err(failed("已有候选资源不一致；保留原文件"));
                }
            }
            return Ok(old);
        }
        fs::rename(stage.path(), &directory).map_err(|_| failed("候选原子发布失败；保留原文件"))?;
        Ok(manifest)
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
        if !matches!(status.phase.as_str(), "running" | "waiting") {
            return Err(failed("监督器正在切换或需要检查，未提交新请求"));
        }
        let candidate = self.journal.join("candidates").join(&a.candidate_sha256);
        let path = candidate.join("manifest.json");
        if fs::symlink_metadata(&candidate)
            .map_err(|_| failed("候选不存在"))?
            .file_type()
            .is_symlink()
        {
            return Err(failed("候选目录不能是链接"));
        }
        let bytes = fs::read(path).map_err(|_| failed("候选清单不可读取"))?;
        if bytes.len() > LIVE_UPGRADE_MAX_JSON {
            return Err(failed("候选清单超过上限"));
        }
        let manifest: Value = serde_json::from_slice(&bytes).map_err(|_| failed("候选清单损坏"))?;
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
