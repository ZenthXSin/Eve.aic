//! 开发动作只在命中具体系统危险时审查；普通操作不以目标或逐步用户授权为门槛。
use super::*;

pub(super) use eve_developer_api::credential_path;

/// 路径已由宿主绑定；只审查操作系统、服务状态与启动配置，普通工程配置不额外送审。
fn sensitive_write(path: &str) -> bool {
    let path = Path::new(path);
    path.is_absolute()
        && [
            "/etc",
            "/boot",
            "/bin",
            "/sbin",
            "/usr/bin",
            "/usr/sbin",
            "/usr/lib",
            "/lib",
            "/lib64",
            "/dev",
            "/proc",
            "/sys",
            "/var/lib",
        ]
        .iter()
        .any(|root| path.starts_with(root))
}

fn github_system_change(arguments: &GithubArguments) -> bool {
    if arguments.method == "GET" {
        return false;
    }
    let resource = arguments.resource.as_str();
    [
        "hooks",
        "collaborators",
        "teams",
        "keys",
        "actions/permissions",
        "branches",
    ]
    .iter()
    .any(|prefix| {
        resource == *prefix
            || resource
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    }) || resource.starts_with("contents/.github/workflows/")
        || resource.starts_with("contents/.env")
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
        let path = self.model_file_path(relative, true)?;
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
        self.authorize_host_paths(operation, context)?;
        self.authorize_file_targets(operation, context)?;
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

    /// 仅非主人的具体系统危险触发模型审查；身份、路径、回执与候选完整性仍由宿主核对。
    pub(super) async fn review_operation(
        &self,
        operation_id: u64,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<ReviewedFiles, ToolExecutionError> {
        let mut paths = Vec::new();
        let mut artifacts = Vec::new();
        let mut baseline = None;
        let system_path = |path: &str| {
            let actual = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                self.root.join(path)
            };
            sensitive_write(&actual.to_string_lossy())
        };
        let action = match operation {
            DeveloperOperation::Command(command) => {
                if !self.config.commands {
                    return Err(failed("命令执行未启用"));
                }
                self.reject_evidence_credentials(
                    &serde_json::to_string(command).map_err(|_| failed("命令无法编码"))?,
                )?;
                let reason = match &self.command_policy {
                    Some(policy) => {
                        let cwd = self.model_path(&command.cwd, false)?;
                        let evaluation = policy.evaluate_at(&command.program, &command.args, &cwd);
                        match evaluation.decision {
                            Some(CommandDecision::Forbidden) => {
                                return Err(failed(format!(
                                    "命令被策略禁止：{}。不得更换工具或写法绕过",
                                    evaluation.reason()
                                )));
                            }
                            Some(CommandDecision::Prompt) => evaluation.reason(),
                            Some(CommandDecision::Allow) => return Ok(ReviewedFiles(vec![], vec![], None, None)),
                            None => return Ok(ReviewedFiles(vec![], vec![], None, None)),
                        }
                    }
                    None => return Err(failed("命令策略未装配，未执行")),
                };
                Some((command.clone(), reason))
            }
            DeveloperOperation::Github(arguments) => {
                let arguments = self.github_arguments(arguments.clone())?;
                let encoded = serde_json::to_string(&arguments).map_err(|_| failed("GitHub 动作无法编码"))?;
                self.reject_evidence_credentials(&encoded)?;
                if !github_system_change(&arguments) {
                    return Ok(ReviewedFiles(vec![], vec![], None, None));
                }
                Some((business_action("github_api", serde_json::to_value(arguments).map_err(|_| failed("GitHub 动作无法编码"))?),
                    "变更仓库访问控制、webhook、运行权限或自动执行配置，核对是否泄露凭据或扩大外部控制面".into()))
            }
            DeveloperOperation::PrepareUpgrade => {
                if !self.config.self_upgrade || !self.config.commands {
                    return Err(failed("自身升级准备未启用"));
                }
                let source = self.source_fingerprint(context).await?;
                let (_, supervisor) = self.supervisor()?;
                // 准备只构建、验证并冻结候选，没有线上切换；源码快照不依赖模型批准。
                return Ok(ReviewedFiles(vec![], vec![], Some((supervisor.active_sha256, supervisor.active_source_revision)), Some(source)));
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
                    "将已验证候选切换为运行中的 Eve；仅核对完整差异是否引入凭据外泄、身份绕过、关键状态破坏或持续服务不可用。监督器验证、租约和回退用于判断实际影响，不要求用户逐步批准".into()))
            }
            DeveloperOperation::RestartSelf => Some((business_action("restart_self",json!({"scope":context.scope(),"turn_id":context.turn_id(),"target":"current-running-binary","retain_state":true})),"重启运行中的 Eve；核对是否会丢失关键状态或中断未结操作。监督器等待回执和空闲后执行的正常短暂重启不属于直接系统危害，不要求逐步用户授权".into())),
            DeveloperOperation::Write(a) if !self.isolated && system_path(&a.path) => {
                paths.push(a.path.clone());
                Some((
                    business_action(
                        "write_file",
                        serde_json::to_value(a).map_err(|_| failed("写入动作无法编码"))?,
                    ),
                    "写入关键系统路径；核对内容是否破坏系统、泄露凭据或扩大控制面"
                        .into(),
                ))
            }
            DeveloperOperation::Edit(a) if !self.isolated && system_path(&a.path) => {
                paths.push(a.path.clone());
                Some((business_action("edit_file", serde_json::to_value(a).map_err(|_| failed("编辑动作无法编码"))?), "编辑关键系统路径；依据原文件与替换内容判断直接系统影响".into()))
            }
            DeveloperOperation::Patch(a) => {
                let touched = eve_apply_patch::paths(&a.patch)
                    .ok_or_else(|| failed("补丁无法解析，未执行"))?;
                if touched.iter().any(|p| credential_path(p)) {
                    return Err(failed("凭据文件不交给模型工具；请通过宿主受信凭据配置处理"));
                }
                if self.isolated || !touched.iter().any(|p| system_path(p)) {
                    return Ok(ReviewedFiles(vec![], vec![], None, None));
                }
                paths = touched;
                Some((business_action("apply_patch", serde_json::to_value(a).map_err(|_| failed("补丁动作无法编码"))?), "补丁触及关键系统路径；依据完整目标、补丁和原文件判断直接系统影响".into()))
            }
            _ => None,
        };
        let Some((mut action, reason)) = action else {
            return Ok(ReviewedFiles(vec![], vec![], None, None));
        };
        // 只读取宿主当前身份；委派与后台沿用原始可信 scope，不接受模型传来的确认。
        // 命令禁止规则、凭据检查和升级候选核对已执行，之后仍复核权限和文件边界。
        if context.scope().is_some_and(|scope| {
            self.authorization
                .as_ref()
                .is_some_and(|authorization| authorization.is_owner(scope))
        }) {
            self.authorize_selected_backend(operation, context)?;
            let files = ReviewedFiles(vec![], artifacts, baseline, None);
            files.recheck(self)?;
            return Ok(files);
        }
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
            self.reject_evidence_credentials(&encoded).map_err(|_| {
                "审核上下文含宿主凭据，未向审核模型发送；请通过受信凭据配置处理".to_owned()
            })?;
            if !self.config.action_review {
                return Err("系统安全审查已关闭，命中具体危险的动作未执行".into());
            }
            let reviewer = self
                .action_reviewer
                .as_ref()
                .ok_or("系统安全审查器未配置，具体危险尚未核对")?;
            context.check_review_evidence().map_err(|_| {
                "review_evidence_changed：用户原话证据已更新或不可核验，旧动作未执行".to_owned()
            })?;
            let verdict = reviewer.review_system_safety(request).await?;
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
        if !verdict.permits_system_safety() {
            return Err(failed(format!(
                "系统安全审查未放行，命令未执行/业务动作未执行：风险 {:?}。{}",
                verdict.risk, verdict.rationale
            )));
        }
        self.authorize_selected_backend(operation, context)?;
        let files = ReviewedFiles(observed, artifacts, baseline, None);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_names_are_not_treated_as_credential_files() {
        for path in [
            "src/credentials.rs",
            "src/secret_parser.rs",
            "src/credentials/mod.rs",
            "tests/secrets/parser.rs",
            "src/credentials",
            "tests/secrets",
            "src/credential_schema.json",
            "docs/secret-handling.txt",
            "docs/secret-handling.md",
            "policy.json",
            "Cargo.toml",
            ".cargo/config.toml",
            ".github/workflows/build.yml",
            "fixtures/hosts.yml",
            "src/auth.json",
        ] {
            assert!(!credential_path(path), "{path}");
            assert!(!sensitive_write(path), "{path}");
        }
        for path in [
            ".env",
            ".env.local",
            ".ssh/id_ed25519",
            "credentials.json",
            "private/key.pem",
            "admin-bindings.json",
            ".netrc",
            ".npmrc",
            ".pgpass",
            ".docker/config.json",
            ".kube/config",
            ".config/gh/hosts.yml",
            ".composer/auth.json",
            ".config/composer/auth.json",
            "/run/secrets/service-token",
        ] {
            assert!(credential_path(path), "{path}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relative_file_targets_use_the_bound_system_root_before_review() {
        let project = tempfile::tempdir().unwrap();
        let ledger = tempfile::tempdir().unwrap();
        let config: DeveloperConfig = serde_json::from_value(json!({
            "workspace":project.path(),"operators":["owner"],"commands":true
        }))
        .unwrap();
        let mut backend = LocalDeveloper::new(config, ledger.path().into(), None).unwrap();
        backend.root = PathBuf::from("/etc");
        let operation = DeveloperOperation::Write(FileArguments {
            path: "hosts".into(),
            offset: 0,
            content: Some("no execution in this test".into()),
            expected_sha256: Some(String::new()),
        });
        // 只调用准入阶段，未配置审核器；不写任何真实系统文件。
        assert!(
            backend
                .review_operation(1, &operation, &ToolExecutionContext::new())
                .await
                .is_err()
        );
        assert!(ledger.path().join("action-reviews.jsonl").is_file());
    }
}
