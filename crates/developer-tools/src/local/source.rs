//! 候选差异以宿主确认的在线提交为基线；完整证据不可脱敏、裁剪或有损解码。
use super::*;

#[derive(PartialEq)]
pub(super) struct SourceEvidence {
    pub fingerprint: Value,
    pub descriptor: Value,
    pub diff: String,
}

pub(super) fn cargo_checks() -> [Vec<&'static str>; 4] {
    [
        vec!["fmt", "--", "--check"],
        vec![
            "clippy",
            "--release",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
        vec!["test", "--release", "--workspace", "--locked"],
        vec!["build", "-p", "eve-app", "--bins", "--release", "--locked"],
    ]
}

pub(super) fn read_source_diff(
    directory: &Path,
    manifest: &Value,
) -> Result<String, ToolExecutionError> {
    let descriptor = &manifest["source_diff"];
    if descriptor
        .as_object()
        .is_none_or(|fields| fields.len() != 4)
        || descriptor["path"] != "source-diff.patch"
        || !descriptor["sha256"]
            .as_str()
            .is_some_and(valid_candidate_sha)
        || !descriptor["base_revision"]
            .as_str()
            .is_some_and(valid_source_revision)
        || !descriptor["target_revision"]
            .as_str()
            .is_some_and(valid_source_revision)
        || descriptor["target_revision"] != manifest["source"]["head"]
        || manifest["source"]["diff"] != hash(b"")
        || manifest["source"]["untracked"] != hash(b"")
    {
        return Err(failed(
            "candidate_changed：候选缺少完整且干净的源码差异证据",
        ));
    }
    let bytes = live_upgrade::read_bounded(
        &directory.join("source-diff.patch"),
        LIVE_UPGRADE_MAX_SOURCE_DIFF,
    )?;
    if descriptor["sha256"] != hash(&bytes) {
        return Err(failed("candidate_changed：源码差异已变化"));
    }
    let text = String::from_utf8(bytes).map_err(|_| failed("源码差异不是完整 UTF-8"))?;
    reviewable_diff(&text)?;
    Ok(text)
}

fn reviewable_diff(text: &str) -> Result<(), ToolExecutionError> {
    if text.lines().any(|line| {
        line == "GIT binary patch"
            || line.starts_with("Binary files ")
            || line.starts_with("Submodule ")
            || line.starts_with("index ") && line.ends_with(" 160000")
            || [
                "new file mode 160000",
                "deleted file mode 160000",
                "old mode 160000",
                "new mode 160000",
            ]
            .contains(&line)
    }) {
        return Err(failed(
            "源码差异包含无法完整审核的二进制或子模块变更，未发布候选",
        ));
    }
    Ok(())
}

fn private_key_marker(text: &str) -> bool {
    // 检查真实 PEM 行，代码或文档中引用检测字符串本身不是密钥材料。
    if text.lines().any(|line| {
        let line = line.trim().trim_start_matches(['+', '-']).trim();
        line.starts_with("BEGIN ") && line.ends_with("PRIVATE KEY-----")
            || line.starts_with("-----BEGIN ") && line.ends_with("PRIVATE KEY-----")
    }) {
        return true;
    }
    fn fields(value: &Value) -> bool {
        match value {
            Value::String(text) => private_key_marker(text),
            Value::Array(values) => values.iter().any(fields),
            Value::Object(values) => values
                .iter()
                .any(|(key, value)| private_key_marker(key) || fields(value)),
            _ => false,
        }
    }
    if serde_json::from_str::<Value>(text).is_ok_and(|value| fields(&value)) {
        return true;
    }
    // 补丁中的 JSON 配置行仍按完整值检查，保留 Git 的 +/- 行标记只会掩盖格式。
    text.lines().any(|line| {
        let Some(line) = line.strip_prefix(['+', '-']) else {
            return false;
        };
        serde_json::from_str::<Value>(line).is_ok_and(|value| fields(&value))
    })
}

impl LocalDeveloper {
    pub(super) fn reject_evidence_credentials(&self, text: &str) -> Result<(), ToolExecutionError> {
        if self
            .token
            .as_ref()
            .is_some_and(|token| text.contains(token))
            || private_key_marker(text)
        {
            return Err(failed(
                "完整证据包含凭据，未保存或发送模型；请通过宿主受信凭据配置处理",
            ));
        }
        Ok(())
    }

    async fn source_git(
        &self,
        args: Vec<String>,
        context: &ToolExecutionContext,
        limit: usize,
    ) -> Result<String, ToolExecutionError> {
        let result = self
            .command_evidence(
                CommandArguments {
                    owner_confirmed: false,
                    program: "git".into(),
                    args: ["-c", "core.fsmonitor=false"]
                        .into_iter()
                        .map(str::to_owned)
                        .chain(args)
                        .collect(),
                    cwd: ".".into(),
                    timeout_ms: Some(30_000),
                },
                context,
                limit,
            )
            .await?;
        if result["success"] != true || result["stdout_truncated"] == true {
            return Err(failed(
                "源码证据无法完整取得或超过审核容量；未裁剪、未发布候选",
            ));
        }
        let text = result["stdout"]
            .as_str()
            .ok_or_else(|| failed("源码证据缺失"))?;
        self.reject_evidence_credentials(text)?;
        Ok(text.to_owned())
    }

    pub(super) async fn source_fingerprint(
        &self,
        context: &ToolExecutionContext,
    ) -> Result<SourceEvidence, ToolExecutionError> {
        let (_, status) = self.supervisor()?;
        let baseline = status
            .active_source_revision
            .as_deref()
            .filter(|value| valid_source_revision(value))
            .ok_or_else(|| failed("宿主未提供可验证的干净在线源码基线；未准备候选"))?;
        let head = self
            .source_git(vec!["rev-parse".into(), "HEAD".into()], context, 256)
            .await?
            .trim()
            .to_owned();
        if !valid_source_revision(&head) {
            return Err(failed("源码 HEAD 不是完整提交摘要"));
        }
        let tracked = self
            .source_git(
                ["ls-files", "-v", "-z"].map(str::to_owned).into(),
                context,
                2 * 1024 * 1024,
            )
            .await?;
        if tracked
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .any(|entry| entry.as_bytes().first() != Some(&b'H'))
        {
            return Err(failed(
                "源码索引含跳过检查、冲突或非普通追踪标记，无法证明完整差异",
            ));
        }
        let untracked = self
            .source_git(
                ["ls-files", "--others", "--exclude-standard", "-z"]
                    .map(str::to_owned)
                    .into(),
                context,
                LIVE_UPGRADE_MAX_SOURCE_DIFF,
            )
            .await?;
        if !untracked.is_empty() {
            return Err(failed(
                "先提交全部新增源码，再准备自身升级；未跟踪文件无法绑定验证证据",
            ));
        }
        let prefix = [
            "--no-pager",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--no-renames",
            "--ignore-submodules=none",
        ];
        let args = |revision: &str| {
            prefix
                .iter()
                .copied()
                .chain([revision, "--"])
                .map(str::to_owned)
                .collect()
        };
        let dirty = self
            .source_git(args("HEAD"), context, LIVE_UPGRADE_MAX_SOURCE_DIFF)
            .await?;
        if !dirty.is_empty() {
            return Err(failed(
                "先提交本地源码修改，再准备候选；在线版本必须可从提交完整重建",
            ));
        }
        // 原始 NUL 路径列表先行检查，不能通过重命名或 Git 引号避开凭据边界。
        let paths = self
            .source_git(
                [
                    "--no-pager",
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-renames",
                    "--ignore-submodules=none",
                    "--name-only",
                    "-z",
                    baseline,
                    "--",
                ]
                .map(str::to_owned)
                .into(),
                context,
                LIVE_UPGRADE_MAX_SOURCE_DIFF,
            )
            .await?;
        if paths
            .split('\0')
            .filter(|path| !path.is_empty())
            .any(review::credential_path)
        {
            return Err(failed(
                "源码变更涉及凭据或身份授权账本，未读取差异或发布候选",
            ));
        }
        let diff = self
            .source_git(args(baseline), context, LIVE_UPGRADE_MAX_SOURCE_DIFF)
            .await?;
        reviewable_diff(&diff)?;
        Ok(SourceEvidence {
            fingerprint: json!({"head":head,"diff":hash(b""),"untracked":hash(b"")}),
            descriptor: json!({"path":"source-diff.patch","sha256":hash(diff.as_bytes()),"base_revision":baseline,"target_revision":head}),
            diff,
        })
    }

    pub(super) fn validate_candidate_review_capacity(
        &self,
        manifest: &Value,
        diff: &str,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        let (_, status) = self.supervisor()?;
        if status.active_source_revision.as_deref()
            != manifest["source_diff"]["base_revision"].as_str()
        {
            return Err(failed("验证期间在线源码基线改变；未发布候选"));
        }
        // 租约各摘要定长，时间用最长 u64；实际签发的完整请求不会比此容量预算更大。
        let lease = json!({"format_version":1,"candidate_sha256":"f".repeat(64),"manifest_sha256":"f".repeat(64),
            "plan_sha256":"f".repeat(64),"validation_sha256":"f".repeat(64),"state_schema_version":1,
            "created_at_ms":u64::MAX,"expires_at_ms":u64::MAX});
        let action = review::business_action(
            "apply_self_upgrade",
            json!({"candidate_sha256":manifest["candidate_sha256"],
            "active_sha256":status.active_sha256,"active_source_revision":status.active_source_revision,"source_diff_content":diff,
            "manifest":manifest,"lease":lease,"scope":context.scope(),"turn_id":context.turn_id(),"retain_state":true}),
        );
        let mut request = review_request(&action, "将已验证候选切换为运行中的 Eve，会短暂中断服务。候选验证不等于用户授权；须按原话审核完整候选、验证记录、运行版本和准备租约；业务权限不能跳过审核、回退及熔断".into(), context)?;
        request.workspace = self.root.to_string_lossy().into_owned();
        request.isolated = self.isolated;
        request.validate_capacity().map_err(|_| {
            failed("完整候选差异、清单、租约及用户原话超过审核容量；未裁剪、未发布候选")
        })?;
        self.reject_evidence_credentials(
            &serde_json::to_string(&request).map_err(|_| failed("候选审核证据编码失败"))?,
        )
    }
}
