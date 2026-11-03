//! 开发操作共用审核入口；完整动作和原话不可用时拒绝，审核不能扩大宿主权限。
use super::*;

pub(super) fn credential_path(path: &str) -> bool {
    Path::new(path).components().any(|part| match part {
        Component::Normal(name) => {
            let name = name.to_string_lossy().to_ascii_lowercase();
            name == ".env"
                || name.starts_with(".env.")
                || matches!(
                    name.as_str(),
                    "private"
                        | ".ssh"
                        | ".aws"
                        | ".gnupg"
                        | "business-permissions.json"
                        | "business-permissions.initialized"
                        | "business-permissions.failed"
                        | "admin-bindings.json"
                )
                || name.contains("secret")
                || name.contains("credential")
                || name.ends_with(".pem")
                || name.ends_with(".key")
                || name.ends_with(".p12")
        }
        _ => false,
    })
}

fn sensitive_write(path: &str) -> bool {
    Path::new(path).components().any(|part| match part {
        Component::Normal(name) => {
            let name = name.to_string_lossy().to_ascii_lowercase();
            matches!(
                name.as_str(),
                "agent.md"
                    | "agents.md"
                    | "rule.md"
                    | "user.md"
                    | "cargo.toml"
                    | "cargo.lock"
                    | ".cargo"
                    | ".github"
            ) || name.contains("config")
                || name.contains("policy")
                || name.contains("authorization")
                || name.contains("admin-bindings")
        }
        _ => false,
    })
}

/// 审核期间文件发生变化时不得使用新正文执行曾审核的旧动作。
pub(super) struct ReviewedFiles(
    Vec<(String, Option<String>)>,
    Vec<(PathBuf, String)>,
    Option<(String, Option<String>)>,
    Option<source::SourceEvidence>,
);
impl ReviewedFiles {
    pub(super) fn into_source(self) -> Option<source::SourceEvidence> {
        self.3
    }
    pub(super) fn recheck(&self, backend: &LocalDeveloper) -> Result<(), ToolExecutionError> {
        for (relative, expected) in &self.0 {
            let (_, actual) = backend.review_file(relative)?;
            if &actual != expected {
                return Err(failed("审核期间目标文件已变化，动作未执行；重新读取并审核"));
            }
        }
        for (path, expected) in &self.1 {
            let bytes = live_upgrade::read_bounded(path, LIVE_UPGRADE_MAX_JSON)?;
            if hash(&bytes) != *expected {
                return Err(failed("审核期间候选清单或准备租约已变化，未提交升级"));
            }
        }
        if let Some((active, revision)) = &self.2 {
            let (_, status) = backend.supervisor()?;
            if status.active_sha256 != *active || status.active_source_revision != *revision {
                return Err(failed(
                    "审核期间在线版本发生变化，未提交升级；重新准备并审核",
                ));
            }
        }
        Ok(())
    }
}

impl LocalDeveloper {
    fn review_file(&self, relative: &str) -> Result<(Value, Option<String>), ToolExecutionError> {
        let path = self.path(relative, true)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file() && metadata.len() <= MAX_ACTION_REVIEW_BYTES as u64 => {}
            Ok(_) => return Err(failed("审核目标无法完整读取，未裁剪正文，动作未执行")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Value::Null, None));
            }
            Err(_) => return Err(failed("审核目标不可读取，动作未执行")),
        };
        let bytes = live_upgrade::read_bounded(&path, MAX_ACTION_REVIEW_BYTES)
            .map_err(|_| failed("审核目标无法完整读取，未裁剪正文，动作未执行"))?;
        let digest = hash(&bytes);
        let text = String::from_utf8(bytes)
            .map_err(|_| failed("审核目标不是完整 UTF-8 文本，动作未执行"))?;
        Ok((json!({"sha256":digest,"content":text}), Some(digest)))
    }

    /// 只按已绑定平台身份复核，绝不在审核后将失权的宿主执行偷偷降级为成员执行。
    pub(super) fn authorize_selected_backend(
        &self,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        context.check_review_evidence()?;
        if context.scope().is_some_and(|scope| {
            self.authorization
                .as_ref()
                .is_some_and(|authorization| !authorization.permits_business(scope))
        }) {
            return Err(ToolExecutionError::Rejected(
                "审核期间业务权限已暂停，动作未执行".into(),
            ));
        }
        if self.isolated {
            if stopped(context)
                || context.scope().is_none_or(|scope| {
                    self.authorization
                        .as_ref()
                        .is_none_or(|a| !a.allows_workspace_development(scope))
                })
            {
                return Err(failed("审核期间成员开发权限已撤销，动作未执行"));
            }
            return Ok(());
        }
        self.authorize_operation(operation, context)?;
        if self.member_operation(operation, context) {
            return Err(failed("审核期间宿主权限已撤销，不能沿用原宿主工作区执行"));
        }
        Ok(())
    }

    /// GitHub 写入、宿主安全配置变更与需确认命令都经过同一独立审核。
    pub(super) async fn review_operation(
        &self,
        operation_id: u64,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<ReviewedFiles, ToolExecutionError> {
        context.check_review_evidence()?;
        let mut paths = Vec::new();
        let mut artifacts = Vec::new();
        let mut baseline = None;
        let mut reviewed_source = None;
        let action = match operation {
            DeveloperOperation::Command(command) => {
                if !self.config.commands {
                    return Err(failed("命令执行未启用"));
                }
                let reason = match &self.command_policy {
                    Some(policy) => {
                        let evaluation = policy.evaluate(&command.program, &command.args);
                        match evaluation.decision {
                            Some(CommandDecision::Forbidden) => {
                                return Err(failed(format!(
                                    "命令被策略禁止：{}。不得更换工具或写法绕过",
                                    evaluation.reason()
                                )));
                            }
                            Some(CommandDecision::Prompt) => evaluation.reason(),
                            Some(CommandDecision::Allow) => return Ok(ReviewedFiles(vec![], vec![], None, None)),
                            None => "命令策略未确认安全范围，必须审核完整命令".into(),
                        }
                    }
                    None => "命令策略未装配，必须审核完整命令".into(),
                };
                Some((command.clone(), reason))
            }
            DeveloperOperation::Github(arguments) => {
                let arguments = self.github_arguments(arguments.clone())?;
                if arguments.method == "GET" {
                    return Ok(ReviewedFiles(vec![], vec![], None, None));
                }
                Some((business_action("github_api", serde_json::to_value(arguments).map_err(|_| failed("GitHub 动作无法编码"))?),
                    "向 GitHub 写入或删除共享数据；须核对仓库、资源、完整请求正文及原话授权，合并/改写引用/删除发行均属实际副作用".into()))
            }
            DeveloperOperation::PrepareUpgrade => {
                if !self.config.self_upgrade || !self.config.commands {
                    return Err(failed("自身升级准备未启用"));
                }
                let source = self.source_fingerprint(context).await?;
                let (_, supervisor) = self.supervisor()?;
                let checks = source::cargo_checks().into_iter().map(|args| json!({"program":self.config.cargo_program,
                    "arguments":self.config.cargo_arguments.iter().cloned().chain(args.into_iter().map(str::to_owned)).collect::<Vec<_>>(),"cwd":"."})).collect::<Vec<_>>();
                let action = business_action("prepare_self_upgrade", json!({"source":source.fingerprint,"source_diff":source.descriptor,
                    "source_diff_content":source.diff,"active_sha256":supervisor.active_sha256,"checks":checks,
                    "after_rust_tests":{"program":if cfg!(windows) {"python"} else {"python3"},"arguments":["-B","scripts/verify-self-upgrade.py"],"cwd":"."},
                    "after_build":{"program":if cfg!(windows) {"python"} else {"python3"},"arguments":["-B","scripts/verify-self-upgrade.py","--binary","<cargo metadata target_directory>/release/eve-qqbot"],"cwd":".","isolated":true,"production_switched":false},
                    "publish_only":true,"activate":false}));
                baseline = Some((supervisor.active_sha256, supervisor.active_source_revision));
                reviewed_source = Some(source);
                Some((action, "构建与测试会在宿主执行候选的 build.rs、测试和监督验收脚本，属于实际代码执行；须按真实原话审核完整源码差异和执行计划。准备只冻结候选，不授权激活；认证、审核、审计、回退和熔断不得被代码关闭".into()))
            }
            DeveloperOperation::ApplyUpgrade(arguments) => {
                if !self.config.self_upgrade || !self.config.commands || !valid_candidate_sha(&arguments.candidate_sha256) {
                    return Err(failed("实时升级未启用或候选摘要无效"));
                }
                let (_, supervisor) = self.supervisor()?;
                let candidate = self.journal.join("candidates").join(&arguments.candidate_sha256);
                if !fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink()) {
                    return Err(failed("候选目录类型无效，未读取或审核"));
                }
                let manifest_path = candidate.join("manifest.json");
                let lease_path = self.journal.join("leases").join(format!("{}.json", arguments.candidate_sha256));
                if !lease_path.exists() {
                    return Err(failed("prepare_lease_missing：请先准备候选"));
                }
                let manifest_bytes = live_upgrade::read_bounded(&manifest_path, LIVE_UPGRADE_MAX_JSON)?;
                let lease_bytes = live_upgrade::read_bounded(&lease_path, LIVE_UPGRADE_MAX_JSON)?;
                let manifest: Value = serde_json::from_slice(&manifest_bytes).map_err(|_| failed("候选清单无效，未审核"))?;
                let lease: Value = serde_json::from_slice(&lease_bytes).map_err(|_| failed("准备租约无效，未审核"))?;
                let source_diff = source::read_source_diff(&candidate, &manifest)?;
                if supervisor.active_source_revision.as_deref() != manifest["source_diff"]["base_revision"].as_str()
                    && !(supervisor.active_candidate_sha256.as_deref() == Some(arguments.candidate_sha256.as_str())
                        && manifest["binary_sha256"] == supervisor.active_sha256) {
                    return Err(failed("候选差异基线与在线版本不同，未审核；重新准备候选"));
                }
                self.reject_evidence_credentials(&source_diff)?;
                artifacts.push((candidate.join("source-diff.patch"), hash(source_diff.as_bytes())));
                artifacts.push((manifest_path, hash(&manifest_bytes)));
                artifacts.push((lease_path, hash(&lease_bytes)));
                baseline = Some((supervisor.active_sha256.clone(), supervisor.active_source_revision.clone()));
                Some((business_action("apply_self_upgrade", json!({"candidate_sha256":arguments.candidate_sha256,"active_sha256":supervisor.active_sha256,"active_source_revision":supervisor.active_source_revision,"source_diff_content":source_diff,"manifest":manifest,"lease":lease,"scope":context.scope(),"turn_id":context.turn_id(),"retain_state":true})),
                    "将已验证候选切换为运行中的 Eve，会短暂中断服务。候选验证不等于用户授权；须按原话审核完整候选、验证记录、运行版本和准备租约；业务权限不能跳过审核、回退及熔断".into()))
            }
            DeveloperOperation::RestartSelf => Some((business_action("restart_self",json!({"scope":context.scope(),"turn_id":context.turn_id(),"target":"current-running-binary","retain_state":true})),"重启当前 Eve 服务会短暂中断服务；必须当前真实用户明确要求并经宿主监督器等待回执和空闲后执行，不能伪称已重启".into())),
            DeveloperOperation::Write(a) if !self.isolated && sensitive_write(&a.path) => {
                paths.push(a.path.clone());
                Some((
                    business_action(
                        "write_file",
                        serde_json::to_value(a).map_err(|_| failed("写入动作无法编码"))?,
                    ),
                    "修改宿主身份、安全策略或工程配置；业务授权不能关闭认证、审核、审计和失败关闭"
                        .into(),
                ))
            }
            DeveloperOperation::Edit(a) if !self.isolated && sensitive_write(&a.path) => {
                paths.push(a.path.clone());
                Some((business_action("edit_file", serde_json::to_value(a).map_err(|_| failed("编辑动作无法编码"))?), "修改宿主身份、安全策略或工程配置；核对完整原文件与替换内容，业务授权不能关闭安全底座".into()))
            }
            DeveloperOperation::Patch(a) => {
                let touched = eve_apply_patch::paths(&a.patch)
                    .ok_or_else(|| failed("补丁无法解析，未执行"))?;
                if touched.iter().any(|p| credential_path(p)) {
                    return Err(failed("凭据文件不交给模型工具；请通过宿主受信凭据配置处理"));
                }
                if self.isolated || !touched.iter().any(|p| sensitive_write(p)) {
                    return Ok(ReviewedFiles(vec![], vec![], None, None));
                }
                paths = touched;
                Some((business_action("apply_patch", serde_json::to_value(a).map_err(|_| failed("补丁动作无法编码"))?), "修改宿主身份、安全策略或工程配置；核对全部补丁及原文件，业务授权不能关闭安全底座".into()))
            }
            _ => None,
        };
        let Some((mut action, reason)) = action else {
            return Ok(ReviewedFiles(vec![], vec![], None, None));
        };
        let mut observed = Vec::new();
        if !paths.is_empty() {
            paths.sort();
            paths.dedup();
            let mut before = serde_json::Map::new();
            for path in paths {
                let (file, digest) = self.review_file(&path)?;
                before.insert(path.clone(), file);
                observed.push((path, digest));
            }
            let mut arguments: Value =
                serde_json::from_str(&action.args[0]).map_err(|_| failed("审核动作无法解析"))?;
            arguments["before"] = Value::Object(before);
            action.args[0] = arguments.to_string();
        }
        let mut request = review_request(&action, reason, context)?;
        request.workspace = self.root.to_string_lossy().into_owned();
        request.isolated = self.isolated;
        let encoded = serde_json::to_string(&request).map_err(|_| failed("审核请求无法编码"))?;
        let fingerprint = hash(encoded.as_bytes());
        let result = async {
            request.validate_capacity()?;
            if request.user_messages.is_empty() {
                return Err(
                    "缺少宿主保存的真实用户原话，owner_confirmed不能代替独立审核或以委托说明授权"
                        .into(),
                );
            }
            self.reject_evidence_credentials(&encoded).map_err(|_| {
                "审核上下文含宿主凭据，未向审核模型发送；请通过受信凭据配置处理".to_owned()
            })?;
            if !self.config.action_review {
                return Err(
                    "独立审核已关闭，需审核的动作不执行；owner_confirmed不能代替审核".into(),
                );
            }
            let reviewer = self
                .action_reviewer
                .as_ref()
                .ok_or("独立审核器未配置，owner_confirmed不能代替审核")?;
            context.check_review_evidence().map_err(|_| {
                "review_evidence_changed：用户原话证据已更新或不可核验，旧动作未执行".to_owned()
            })?;
            let verdict = reviewer.review(request).await?;
            context.check_review_evidence().map_err(|_| {
                "review_evidence_changed：审核期间用户原话证据已更新或不可核验，旧动作未执行"
                    .to_owned()
            })?;
            Ok(verdict)
        }
        .await;
        self.record_review(operation_id, &fingerprint, &result)?;
        let verdict = result.map_err(|e| {
            let reason = format!("行动审核未完成，命令未执行/业务动作未执行：{e}");
            if e.starts_with("review_evidence_changed") {
                ToolExecutionError::Rejected(reason)
            } else {
                failed(reason)
            }
        })?;
        if !verdict.permits() {
            return Err(failed(format!(
                "行动审核未放行，命令未执行/业务动作未执行：风险 {:?}，授权 {:?}。{}",
                verdict.risk, verdict.authorization, verdict.rationale
            )));
        }
        self.authorize_selected_backend(operation, context)?;
        let files = ReviewedFiles(observed, artifacts, baseline, reviewed_source);
        files.recheck(self)?;
        Ok(files)
    }
}

pub(super) fn business_action(name: &str, arguments: Value) -> CommandArguments {
    CommandArguments {
        program: format!("eve.{name}"),
        args: vec![arguments.to_string()],
        cwd: ".".into(),
        timeout_ms: None,
        owner_confirmed: false,
    }
}
