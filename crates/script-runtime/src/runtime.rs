use crate::{
    AuthorizedToolDispatch, Bubblewrap, Evidence, MAX_MESSAGE_BYTES, Preflight, PublishedVersion,
    Receipt, ReceiptStatus, Request, Result, ScriptStore, StateSnapshot, VersionRef, digest, fail,
    reject, store::strict_json,
};
use eve_llm_api::{ContextScope, ToolCall, ToolExecutionContext, ToolExecutionError};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// publish/activate 不会改变现有句柄。私有字段防止调用方改写版本源码。
#[derive(Clone)]
pub struct BoundVersion {
    root: VersionRef,
    owner: ContextScope,
    versions: Arc<BTreeMap<String, PublishedVersion>>,
}
impl BoundVersion {
    pub fn reference(&self) -> &VersionRef {
        &self.root
    }
}

pub struct ScriptRuntime {
    pub(crate) store: Arc<ScriptStore>,
    dispatch: Arc<dyn AuthorizedToolDispatch>,
    pub(crate) sandbox: Bubblewrap,
    artifacts: PathBuf,
}
struct Chain {
    work: tempfile::TempDir,
    promote: Mutex<BTreeSet<String>>,
    root_call_id: String,
    deadline: Instant,
}
fn cargo_recipe(spec: &crate::ScriptSpec) -> Result<Option<(crate::CargoSettings, String)>> {
    match &spec.language {
        crate::Language::RustCargo(settings) => Ok(Some((settings.clone(), spec.source.clone()))),
        crate::Language::Rust => Ok(crate::cargo_model::decoded_source(&spec.source)?
            .map(|(settings, source)| (settings, source.into()))),
        _ => Ok(None),
    }
}
struct InvocationGuard {
    store: Arc<ScriptStore>,
    owner: ContextScope,
    call_id: String,
    armed: bool,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if self.armed {
            // 被宿主drop的future无法再await，但须立即封存未知状态；写失败由Store毒化保护。
            let _ = self.store.interrupted(&self.owner, &self.call_id);
        }
    }
}
impl ScriptRuntime {
    pub fn new(
        store: Arc<ScriptStore>,
        dispatch: Arc<dyn AuthorizedToolDispatch>,
        sandbox: Bubblewrap,
        artifacts: impl Into<PathBuf>,
    ) -> Self {
        Self {
            store,
            dispatch,
            sandbox,
            artifacts: artifacts.into(),
        }
    }
    /// 兼容入口；需要结构化编译诊断的宿主使用 prepare_candidate。
    pub async fn publish_candidate(
        &self,
        spec: crate::ScriptSpec,
        context: ToolExecutionContext,
    ) -> Result<PublishedVersion> {
        match self.prepare_candidate(spec, context).await? {
            crate::CandidatePreparation::Ready(version) => Ok(*version),
            crate::CandidatePreparation::CompilationFailed(report) => Err(reject(format!(
                "Rust隔离编译失败；旧版本保持可用；退出码 {:?}，信号 {:?}；诊断{}：{}",
                report.exit_code,
                report.signal,
                if report.truncated {
                    "（已截断）"
                } else {
                    ""
                },
                report.diagnostics
            ))),
        }
    }
    /// Rust 候选隔离编译失败时返回实际诊断，不写入版本、不改活动状态。
    pub async fn prepare_candidate(
        &self,
        mut spec: crate::ScriptSpec,
        context: ToolExecutionContext,
    ) -> Result<crate::CandidatePreparation> {
        let owner = context.scope().ok_or_else(|| reject("发布缺少可信身份"))?;
        spec.validate()?;
        let cargo = cargo_recipe(&spec)?;
        if let Some((mut settings, source)) = cargo {
            let compiler = self
                .sandbox
                .cargo
                .as_ref()
                .ok_or_else(|| reject("未配置可信离线Cargo编译器"))?;
            match compiler
                .compile(&settings.project(&source), context.clone())
                .await?
            {
                crate::CargoCompilation::Ready(artifact) => {
                    settings.artifact_sha256 = Some(artifact.sha256().into());
                    spec.source = crate::cargo_model::encoded_source(&settings, &source)?;
                    spec.language = crate::Language::Rust;
                }
                crate::CargoCompilation::Failed(report) => {
                    let mut diagnostics = format!(
                        "{} [{}]: {}",
                        report.code,
                        report.stage.as_deref().unwrap_or("unknown"),
                        report.diagnostics
                    );
                    let mut end = diagnostics.len().min(8192);
                    while !diagnostics.is_char_boundary(end) {
                        end -= 1;
                    }
                    let truncated = report.truncated || end < diagnostics.len();
                    diagnostics.truncate(end);
                    return Ok(crate::CandidatePreparation::CompilationFailed(
                        crate::CompilationFailure {
                            language: crate::Language::RustCargo(settings),
                            exit_code: report.exit_code,
                            signal: None,
                            diagnostics,
                            truncated,
                        },
                    ));
                }
            }
            check_stop(
                &context,
                context
                    .deadline()
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(30)),
            )?;
        } else if spec.language == crate::Language::Rust {
            let deadline = context
                .deadline()
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
            match self
                .sandbox
                .prepare_rust_report(&spec.source, &context, deadline)
                .await?
            {
                crate::compiler::RustPreparation::Ready(_) => {}
                crate::compiler::RustPreparation::CompilationFailed(report) => {
                    return Ok(crate::CandidatePreparation::CompilationFailed(report));
                }
            }
            check_stop(&context, deadline)?;
        }
        Ok(crate::CandidatePreparation::Ready(Box::new(
            self.store.publish(owner, spec)?,
        )))
    }
    pub fn bind(&self, owner: &ContextScope, reference: &VersionRef) -> Result<BoundVersion> {
        let mut versions = BTreeMap::new();
        self.collect(owner, reference, &mut versions, &mut BTreeSet::new(), 0)?;
        Ok(BoundVersion {
            root: reference.clone(),
            owner: owner.clone(),
            versions: Arc::new(versions),
        })
    }
    fn collect(
        &self,
        owner: &ContextScope,
        reference: &VersionRef,
        versions: &mut BTreeMap<String, PublishedVersion>,
        visiting: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<()> {
        if depth > 8 {
            return Err(reject("脚本依赖深度超过8"));
        }
        if versions.contains_key(&reference.digest) {
            return Ok(());
        }
        if !visiting.insert(reference.digest.clone()) {
            return Err(reject("脚本依赖递归"));
        }
        let version = self.store.version(owner, reference)?;
        for child in version.spec.dependencies.values() {
            self.collect(owner, child, versions, visiting, depth + 1)?;
        }
        visiting.remove(&reference.digest);
        versions.insert(reference.digest.clone(), version);
        Ok(())
    }
    /// 完整声明图的只读预检；不建临时目录、不扣预算、不写账本、不启动脚本、不调叶子。
    /// 只检查隔离器配置是否存在；内核是否允许 namespace 在真实启动时确认，失败关闭。
    pub fn preflight(
        &self,
        bound: &BoundVersion,
        context: &ToolExecutionContext,
    ) -> Result<Preflight> {
        identity(bound, context)?;
        check_stop(context, Instant::now() + Duration::from_secs(30))?;
        if context.shared_steps().is_none() {
            return Err(reject("脚本调用必须共享宿主原子预算"));
        }
        let mut tools = BTreeSet::new();
        for version in bound.versions.values() {
            let language = cargo_recipe(&version.spec)?
                .map(|(settings, _)| crate::Language::RustCargo(settings))
                .unwrap_or_else(|| version.spec.language.clone());
            self.sandbox.preflight(&language)?;
            self.store.state(&bound.owner, &version.reference)?;
            for name in &version.spec.tools {
                self.dispatch.preflight_tool(name, context)?;
                tools.insert(name.clone());
            }
        }
        Ok(Preflight {
            root: bound.root.clone(),
            versions: bound
                .versions
                .values()
                .map(|v| v.reference.clone())
                .collect(),
            tools: tools.into_iter().collect(),
            dry_run: true,
            sandbox: self.sandbox.description().into(),
        })
    }
    pub async fn run(
        &self,
        bound: &BoundVersion,
        call_id: &str,
        arguments: Value,
        context: ToolExecutionContext,
    ) -> Result<Receipt> {
        crate::model::valid_name(call_id)?;
        if serde_json::to_vec(&arguments).map_err(reject)?.len() > MAX_MESSAGE_BYTES {
            return Err(reject("脚本输入超限"));
        }
        self.preflight(bound, &context)?;
        let deadline = context
            .deadline()
            .map(|d| d.min(Instant::now() + Duration::from_secs(30)))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        let context = context.with_deadline(Some(deadline));
        let chain = Arc::new(Chain {
            work: tempfile::tempdir().map_err(fail)?,
            promote: Mutex::new(BTreeSet::new()),
            root_call_id: call_id.into(),
            deadline,
        });
        self.frame(
            bound,
            bound.root.clone(),
            call_id.into(),
            None,
            arguments,
            context,
            chain,
            0,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    fn frame<'a>(
        &'a self,
        bound: &'a BoundVersion,
        reference: VersionRef,
        call_id: String,
        parent_call_id: Option<String>,
        arguments: Value,
        context: ToolExecutionContext,
        chain: Arc<Chain>,
        depth: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Receipt>> + Send + 'a>> {
        Box::pin(async move {
            identity(bound, &context)?;
            check_stop(&context, chain.deadline)?;
            if depth > 8 {
                return Err(reject("脚本调用深度超过8"));
            }
            reserve(&context)?;
            let version = bound
                .versions
                .get(&reference.digest)
                .ok_or_else(|| reject("句柄没有该依赖版本"))?;
            for name in &version.spec.tools {
                self.dispatch.preflight_tool(name, &context)?;
            }
            let mut receipt = Receipt {
                call_id: call_id.clone(),
                parent_call_id,
                root_call_id: chain.root_call_id.clone(),
                owner: bound.owner.clone(),
                version: reference,
                request_digest: digest(&arguments)?,
                status: ReceiptStatus::Running,
                result: None,
                evidence: Vec::new(),
                error: None,
                failure_diagnostic: None,
            };
            let snapshot = self.store.begin(receipt.clone())?;
            let mut invocation = InvocationGuard {
                store: self.store.clone(),
                owner: bound.owner.clone(),
                call_id: call_id.clone(),
                armed: true,
            };
            let mut pending_state = None;
            let outcome = self
                .execute(
                    bound,
                    version,
                    &arguments,
                    &snapshot,
                    &context,
                    chain.clone(),
                    depth,
                    &mut receipt,
                    &mut pending_state,
                )
                .await;
            match outcome {
                Ok(result) => {
                    receipt.status = ReceiptStatus::Succeeded;
                    receipt.result = Some(result);
                }
                Err(error) => {
                    receipt.status = if receipt.evidence.iter().any(|e| {
                        matches!(e.status, ReceiptStatus::Running | ReceiptStatus::Unknown)
                    }) {
                        ReceiptStatus::Unknown
                    } else {
                        match error {
                            ToolExecutionError::Rejected(_)
                                if !receipt
                                    .evidence
                                    .iter()
                                    .any(|e| e.status == ReceiptStatus::Succeeded) =>
                            {
                                ReceiptStatus::Rejected
                            }
                            ToolExecutionError::Cancelled(_) => ReceiptStatus::Cancelled,
                            _ => ReceiptStatus::Failed,
                        }
                    };
                    // 只记录错误类别；叶子消息可能含凭据，留在原工具审计入口核对。
                    receipt.error = Some(
                        match error {
                            ToolExecutionError::Rejected(_) => "执行被拒绝",
                            ToolExecutionError::Cancelled(_) => "任务已取消或超时",
                            _ => "执行失败；查阅操作证据",
                        }
                        .into(),
                    );
                }
            }
            if depth == 0
                && receipt.status == ReceiptStatus::Succeeded
                && let Err(_error) = self.promote(&chain, &mut receipt)
            {
                receipt.status = ReceiptStatus::Failed;
                receipt.error = Some("产物提升失败；未提交脚本状态".into());
            }
            let state = if receipt.status == ReceiptStatus::Succeeded {
                pending_state
            } else {
                None
            };
            // 状态与终结回执同一持久提交；失败时保留 running，重启转 unknown。
            self.store.complete(&receipt, state)?;
            invocation.armed = false;
            Ok(receipt)
        })
    }
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        bound: &BoundVersion,
        version: &PublishedVersion,
        arguments: &Value,
        state: &StateSnapshot,
        context: &ToolExecutionContext,
        chain: Arc<Chain>,
        depth: usize,
        receipt: &mut Receipt,
        pending_state: &mut Option<Value>,
    ) -> Result<Value> {
        let mut source = tempfile::NamedTempFile::new().map_err(fail)?;
        std::io::Write::write_all(&mut source, version.spec.source.as_bytes()).map_err(fail)?;
        let cargo = cargo_recipe(&version.spec)?;
        let cargo_artifact = if let Some((settings, source)) = &cargo {
            let compiler = self
                .sandbox
                .cargo
                .as_ref()
                .ok_or_else(|| reject("未配置可信离线Cargo编译器"))?;
            match compiler
                .compile(&settings.project(source), context.clone())
                .await?
            {
                crate::CargoCompilation::Ready(artifact) => {
                    if settings.artifact_sha256.as_deref() != Some(artifact.sha256()) {
                        return Err(reject(
                            "Cargo重建产物与固定版本摘要不同，未执行、未自动替换",
                        ));
                    }
                    Some(artifact)
                }
                crate::CargoCompilation::Failed(_) => {
                    return Err(reject(
                        "Cargo固定版本重建失败，未执行；使用候选准备入口查看编译诊断",
                    ));
                }
            }
        } else {
            None
        };
        let compiled = if cargo.is_none() && version.spec.language == crate::Language::Rust {
            Some(
                self.sandbox
                    .prepare_rust(&version.spec.source, context, chain.deadline)
                    .await?,
            )
        } else {
            None
        };
        let executable = if let Some(artifact) = &cargo_artifact {
            artifact.verified_binary()?
        } else {
            compiled
                .as_ref()
                .map(|compiled| compiled.binary.as_path())
                .unwrap_or(source.path())
        };
        let mut child = self.sandbox.spawn(version, executable, chain.work.path())?;
        let mut input = child.stdin.take().ok_or_else(|| fail("子进程缺少 stdin"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| fail("子进程缺少 stdout"))?;
        let mut output = BufReader::new(output);
        send(&mut input, &json!({"protocol":1,"call_id":receipt.call_id,"root_call_id":receipt.root_call_id,"version":receipt.version,"arguments":arguments,"state":state,"workspace":"/work"}), context, chain.deadline).await?;
        let mut count = 0usize;
        loop {
            check_stop(context, chain.deadline)?;
            count += 1;
            if count > 128 {
                return Err(reject("脚本 RPC 数超过128"));
            }
            let mut bytes = Vec::new();
            // take 防止没有换行的恶意 stdout 无限分配；所有协议消息有界。
            let read = async {
                let mut limited = (&mut output).take((MAX_MESSAGE_BYTES + 1) as u64);
                limited.read_until(b'\n', &mut bytes).await.map_err(fail)
            };
            let length = guarded(read, context, chain.deadline).await?;
            if length == 0 || length > MAX_MESSAGE_BYTES || !bytes.ends_with(b"\n") {
                return Err(fail("进程退出、隔离启动失败或协议输出超限"));
            }
            let request: Request = serde_json::from_value(strict_json(&bytes)?).map_err(reject)?;
            let response = match request {
                Request::Return { value } => {
                    drop(input);
                    let exit = guarded(
                        async { child.wait().await.map_err(fail) },
                        context,
                        chain.deadline,
                    )
                    .await?;
                    if !exit.success() {
                        receipt.failure_diagnostic = Some(crate::FailureDiagnostic {
                            stage: crate::FailureStage::AfterReturn,
                            exit_code: exit.code(),
                        });
                        return Err(fail("进程返回后异常退出"));
                    }
                    return Ok(value);
                }
                Request::State { value } => {
                    version.spec.state_schema.validate(&value)?;
                    *pending_state = Some(value);
                    json!({"staged":true})
                }
                Request::Write { path, content } => {
                    let path = workspace_path(chain.work.path(), &path)?;
                    let total = std::fs::read_dir(chain.work.path())
                        .map_err(fail)?
                        .try_fold(0u64, |sum, entry| -> Result<u64> {
                            Ok(sum + entry.map_err(fail)?.metadata().map_err(fail)?.len())
                        })?;
                    if content.len() > MAX_MESSAGE_BYTES
                        || total + content.len() as u64 > 8 * 1024 * 1024
                    {
                        return Err(reject("共享临时文件总量超过8MiB"));
                    }
                    let count = std::fs::read_dir(chain.work.path()).map_err(fail)?.count();
                    if count >= 64 && !path.exists() {
                        return Err(reject("临时文件数超过64"));
                    }
                    std::fs::write(&path, content.as_bytes()).map_err(fail)?;
                    json!({"path":format!("/work/{}",path.file_name().unwrap_or_default().to_string_lossy()),"bytes":content.len()})
                }
                Request::Artifact { path } => {
                    let resolved = workspace_path(chain.work.path(), &path)?;
                    if !resolved.is_file() {
                        return Err(reject("产物文件不存在"));
                    }
                    chain.promote.lock().map_err(fail)?.insert(path);
                    json!({"staged":true,"promoted":false})
                }
                Request::CheckTool { name, arguments } => {
                    if !version.spec.tools.contains(&name) {
                        return Err(reject("脚本未声明该叶子工具"));
                    }
                    let arguments = resolve_workspace(arguments, chain.work.path())?;
                    let call = ToolCall {
                        id: format!("{}-check-{count}", receipt.call_id),
                        name,
                        arguments,
                    };
                    // 与实际执行使用同一参数/身份策略；不能把失败或取消当作普通拒绝，
                    // 更不能把预检成功当作后续执行的权限租约。
                    let outcome = self.dispatch.preflight_call(&call, context);
                    check_stop(context, chain.deadline)?;
                    let allowed = match outcome {
                        Ok(()) => true,
                        Err(ToolExecutionError::Rejected(_)) => false,
                        Err(error) => return Err(error),
                    };
                    json!({"allowed":allowed,"executed":false,"authorization_reserved":false,
                        "reason":if allowed {"preflight_passed"} else {"rejected"}})
                }
                Request::Tool { name, arguments } => {
                    if !version.spec.tools.contains(&name) {
                        return Err(reject("脚本未声明该叶子工具"));
                    }
                    let arguments = resolve_workspace(arguments, chain.work.path())?;
                    let call = ToolCall {
                        id: format!("{}-tool-{count}", receipt.call_id),
                        name: name.clone(),
                        arguments,
                    };
                    self.dispatch.preflight_call(&call, context)?;
                    check_stop(context, chain.deadline)?;
                    reserve(context)?;
                    receipt.evidence.push(Evidence {
                        operation: "tool".into(),
                        target: name,
                        status: ReceiptStatus::Running,
                        result_digest: None,
                    });
                    self.store.record(receipt)?;
                    let outcome = guarded(
                        self.dispatch.dispatch(call, context.clone()),
                        context,
                        chain.deadline,
                    )
                    .await;
                    let evidence = receipt.evidence.last_mut().expect("just inserted");
                    match outcome {
                        Ok(value) => {
                            evidence.status =
                                if value.get("success").and_then(Value::as_bool) == Some(false) {
                                    ReceiptStatus::Failed
                                } else {
                                    ReceiptStatus::Succeeded
                                };
                            evidence.result_digest = Some(digest(&value)?);
                            self.store.record(receipt)?;
                            if value.get("success").and_then(Value::as_bool) == Some(false) {
                                return Err(fail("叶子工具确认失败"));
                            }
                            value
                        }
                        Err(error) => {
                            evidence.status = if matches!(error, ToolExecutionError::Rejected(_)) {
                                ReceiptStatus::Rejected
                            } else {
                                ReceiptStatus::Unknown
                            };
                            self.store.record(receipt)?;
                            return Err(error);
                        }
                    }
                }
                Request::Script {
                    dependency,
                    arguments,
                } => {
                    let reference = version
                        .spec
                        .dependencies
                        .get(&dependency)
                        .cloned()
                        .ok_or_else(|| reject("未声明的脚本依赖"))?;
                    let child_id = format!("call-{}", &digest(&(&receipt.call_id, count))?[..32]);
                    receipt.evidence.push(Evidence {
                        operation: "script".into(),
                        target: child_id.clone(),
                        status: ReceiptStatus::Running,
                        result_digest: None,
                    });
                    self.store.record(receipt)?;
                    let outcome = self
                        .frame(
                            bound,
                            reference,
                            child_id,
                            Some(receipt.call_id.clone()),
                            arguments,
                            context.clone(),
                            chain.clone(),
                            depth + 1,
                        )
                        .await;
                    let evidence = receipt.evidence.last_mut().expect("just inserted");
                    match outcome {
                        Ok(child_receipt) => {
                            evidence.status = child_receipt.status.clone();
                            evidence.result_digest = Some(digest(&child_receipt)?);
                            self.store.record(receipt)?;
                            if child_receipt.status != ReceiptStatus::Succeeded {
                                return Err(fail("依赖脚本未成功，停止调用链"));
                            }
                            child_receipt.result.unwrap_or(Value::Null)
                        }
                        Err(error) => {
                            evidence.status = ReceiptStatus::Unknown;
                            self.store.record(receipt)?;
                            return Err(error);
                        }
                    }
                }
            };
            child.sync_work(chain.work.path())?;
            send(
                &mut input,
                &json!({"ok":true,"result":response}),
                context,
                chain.deadline,
            )
            .await?;
        }
    }
    fn promote(&self, chain: &Chain, receipt: &mut Receipt) -> Result<()> {
        let files = chain.promote.lock().map_err(fail)?;
        if files.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.artifacts).map_err(fail)?;
        let staging = tempfile::tempdir_in(&self.artifacts).map_err(fail)?;
        for name in files.iter() {
            let source = workspace_path(chain.work.path(), name)?;
            let data = std::fs::read(&source).map_err(fail)?;
            let target = staging.path().join(name);
            std::fs::write(&target, &data).map_err(fail)?;
            std::fs::File::open(&target)
                .map_err(fail)?
                .sync_all()
                .map_err(fail)?;
            receipt.evidence.push(Evidence {
                operation: "artifact".into(),
                target: format!("{}/{name}", chain.root_call_id),
                status: ReceiptStatus::Succeeded,
                result_digest: Some(digest(&data)?),
            });
        }
        let target = self.artifacts.join(&chain.root_call_id);
        if target.exists() {
            return Err(fail("产物目标已存在，未覆盖"));
        }
        std::fs::rename(staging.path(), target).map_err(fail)?;
        std::fs::File::open(&self.artifacts)
            .map_err(fail)?
            .sync_all()
            .map_err(fail)
    }
}
fn identity(bound: &BoundVersion, context: &ToolExecutionContext) -> Result<()> {
    if context.scope() != Some(&bound.owner) {
        return Err(reject("脚本句柄与可信调用身份不同"));
    }
    Ok(())
}
pub(crate) fn reserve(context: &ToolExecutionContext) -> Result<()> {
    if !context
        .shared_steps()
        .ok_or_else(|| reject("缺少共享预算"))?
        .reserve(1)
        .map_err(fail)?
    {
        return Err(reject("调用链共享工具预算不足"));
    }
    Ok(())
}
pub(crate) fn check_stop(context: &ToolExecutionContext, deadline: Instant) -> Result<()> {
    if context.is_cancel_requested()
        || Instant::now() >= deadline
        || context.deadline().is_some_and(|d| Instant::now() >= d)
    {
        return Err(ToolExecutionError::Cancelled("调用链已取消或到期".into()));
    }
    Ok(())
}
pub(crate) async fn guarded<T>(
    future: impl Future<Output = Result<T>>,
    context: &ToolExecutionContext,
    deadline: Instant,
) -> Result<T> {
    tokio::pin!(future);
    loop {
        tokio::select! { result = &mut future => return result, _ = tokio::time::sleep(Duration::from_millis(10)) => { check_stop(context, deadline)?; } }
    }
}
pub(crate) async fn send(
    input: &mut tokio::process::ChildStdin,
    value: &Value,
    context: &ToolExecutionContext,
    deadline: Instant,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value).map_err(fail)?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(fail("宿主响应超过进程协议上限"));
    }
    bytes.push(b'\n');
    guarded(
        async { input.write_all(&bytes).await.map_err(fail) },
        context,
        deadline,
    )
    .await
}
fn workspace_path(root: &Path, path: &str) -> Result<PathBuf> {
    if path.is_empty()
        || path.len() > 128
        || Path::new(path).components().count() != 1
        || !matches!(
            Path::new(path).components().next(),
            Some(Component::Normal(_))
        )
        || path
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')))
    {
        return Err(reject("临时文件必须是单层安全相对文件名"));
    }
    let result = root.join(path);
    if result
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(reject("临时文件不允许符号链接"));
    }
    Ok(result)
}
fn resolve_workspace(value: Value, root: &Path) -> Result<Value> {
    match value {
        Value::Object(object) if object.contains_key("$workspace") => {
            if object.len() != 1 {
                return Err(reject("workspace引用不能混合其他字段"));
            }
            let path = workspace_path(
                root,
                object["$workspace"]
                    .as_str()
                    .ok_or_else(|| reject("workspace路径类型无效"))?,
            )?;
            if !path.is_file() {
                return Err(reject("workspace文件不存在"));
            }
            Ok(json!(path))
        }
        Value::Object(object) => Ok(Value::Object(
            object
                .into_iter()
                .map(|(k, v)| Ok((k, resolve_workspace(v, root)?)))
                .collect::<Result<_>>()?,
        )),
        Value::Array(array) => Ok(Value::Array(
            array
                .into_iter()
                .map(|v| resolve_workspace(v, root))
                .collect::<Result<_>>()?,
        )),
        other => Ok(other),
    }
}
