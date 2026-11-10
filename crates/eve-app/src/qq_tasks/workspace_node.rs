//! 实际工作区的冻结 Node 测试组合：输入每次采集，验收只引用真实运行器回执。
//! 运行历史与当前只读观察分离；不重跑测试来形成收尾观察，不从模型 JSON 赋权。
use eve_developer_api::{
    DeveloperBackend, WorkspaceSnapshot, WorkspaceSnapshotContents, WorkspaceSnapshotRequest,
};
use eve_llm_api::{
    ModelSession, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolResult, ToolValidationError, TurnCheckpoint,
};
use eve_script_runtime::{
    FrozenNodeInputContents, FrozenNodeInputEntry, FrozenNodeOutcome, FrozenNodeTestBundle,
    FrozenNodeTestReceipt, FrozenNodeTestRunner,
};
use eve_task_api::{
    TaskCompletionContract, TaskError, TaskFrozenNodeOutcome, TaskOwner, TaskResult,
    WORKSPACE_NODE_TEST_TOOL, WorkspaceNodeEvidence, WorkspaceNodeEvidenceSource,
    WorkspaceObservation, task_strategy_digest, validate_owner, validate_workspace_node_evidence,
    validate_workspace_node_selector,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const MAX_INVOCATIONS: usize = 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceNodeRequest {
    #[serde(default = "default_directory")]
    pub directory: String,
    pub entry: String,
    #[serde(default)]
    pub args: Vec<String>,
}
fn default_directory() -> String {
    ".".into()
}
impl WorkspaceNodeRequest {
    fn parse(value: &Value) -> Result<Self, ToolValidationError> {
        let invalid = || ToolValidationError {
            message: "参数须为 directory（默认 .）、JS 入口 entry 与 args 数组；路径须在工作区内"
                .into(),
        };
        let parsed: Self = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        validate_workspace_node_selector(&parsed.directory, &parsed.entry, &parsed.args)
            .map_err(|_| invalid())?;
        Ok(parsed)
    }
}
struct LedgerReceipt {
    evidence: WorkspaceNodeEvidence,
    receipt: FrozenNodeTestReceipt,
}
#[derive(Default)]
struct Ledger {
    admitted: BTreeSet<String>,
    receipts: BTreeMap<String, LedgerReceipt>,
}
pub struct WorkspaceNodeAdapter {
    runner: FrozenNodeTestRunner,
    backend: Arc<dyn DeveloperBackend>,
    ledger: Mutex<Ledger>,
}
fn rejected(message: &str) -> ToolExecutionError {
    ToolExecutionError::Rejected(message.into())
}
fn unavailable() -> ToolExecutionError {
    ToolExecutionError::Failed("工作区测试见证账本不可用；不宣称完成".into())
}
fn check_stop(context: &ToolExecutionContext) -> Result<(), ToolExecutionError> {
    if context.is_cancel_requested() || context.deadline().is_some_and(|at| Instant::now() >= at) {
        return Err(ToolExecutionError::Cancelled(
            "工作区测试已取消或超过原期限".into(),
        ));
    }
    Ok(())
}
fn context_identity(
    context: &ToolExecutionContext,
) -> Result<(TaskOwner, ModelSession), ToolExecutionError> {
    check_stop(context)?;
    let scope = context
        .scope()
        .ok_or_else(|| rejected("工作区测试缺少可信身份"))?;
    let owner = TaskOwner {
        session_id: scope.session_id.clone(),
        user_id: scope.user_id.clone(),
    };
    validate_owner(&owner).map_err(|_| rejected("工作区测试身份无效"))?;
    let session = context
        .model_session()
        .filter(|session| session.turn_id().is_some())
        .ok_or_else(|| rejected("工作区测试缺少原模型会话与轮次"))?;
    session
        .validate_scope(Some(scope))
        .map_err(|_| rejected("工作区测试模型会话与身份不符"))?;
    if context.shared_steps().is_none() {
        return Err(rejected("工作区测试缺少原共享预算"));
    }
    Ok((owner, session.clone()))
}
fn execution_id(session: &ModelSession) -> String {
    task_strategy_digest(&("eve.workspace_node.execution.v1", session))
}
fn invocation_id(owner: &TaskOwner, session: &ModelSession, call_id: &str) -> String {
    // 工具上下文带模型请求序号，检查点保存原执行会话；二者共享 scope、fork、turn。
    // call_id 在整个检查点唯一；消除请求序号也防止同调用换序号再次准入。
    task_strategy_digest(&(
        "eve.workspace_node.call.v1",
        owner,
        session.for_request(0),
        call_id,
    ))
}

/// 只接受可信后端返回的身份及规范目录；此摘要是工作区身份，不是输入字节摘要。
pub fn workspace_identity(snapshot: &WorkspaceSnapshot) -> Result<String, ToolExecutionError> {
    let owner = TaskOwner {
        session_id: snapshot.scope.session_id.clone(),
        user_id: snapshot.scope.user_id.clone(),
    };
    validate_owner(&owner).map_err(|_| rejected("工作区快照身份无效"))?;
    let canonical = |path: &Path| {
        path.is_absolute()
            && path
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
    };
    if !canonical(&snapshot.workspace_root)
        || !canonical(&snapshot.root)
        || !snapshot.root.starts_with(&snapshot.workspace_root)
    {
        return Err(rejected("快照不属于身份绑定的规范工作区"));
    }
    Ok(task_strategy_digest(&(
        "eve.workspace_node.workspace.v1",
        owner,
        &snapshot.workspace_root,
        &snapshot.root,
    )))
}
fn bundle_from_snapshot(
    snapshot: WorkspaceSnapshot,
    entry: &str,
    args: &[String],
) -> Result<FrozenNodeTestBundle, ToolExecutionError> {
    let input = snapshot
        .entries
        .into_iter()
        .map(|item| FrozenNodeInputEntry {
            path: item.path,
            executable_bits: item.executable_bits,
            contents: match item.contents {
                WorkspaceSnapshotContents::Directory => FrozenNodeInputContents::Directory,
                WorkspaceSnapshotContents::File(bytes) => FrozenNodeInputContents::File(bytes),
            },
        })
        .collect();
    FrozenNodeTestBundle::from_entries(input, entry.into(), args.to_vec())
}
impl WorkspaceNodeAdapter {
    pub fn new(runner: FrozenNodeTestRunner, backend: Arc<dyn DeveloperBackend>) -> Arc<Self> {
        Arc::new(Self {
            runner,
            backend,
            ledger: Mutex::default(),
        })
    }
    async fn snapshot(
        &self,
        directory: &str,
        context: ToolExecutionContext,
    ) -> Result<WorkspaceSnapshot, ToolExecutionError> {
        let (owner, _) = context_identity(&context)?;
        let snapshot = self
            .backend
            .capture_workspace_snapshot(
                WorkspaceSnapshotRequest {
                    root: directory.into(),
                },
                context.clone(),
            )
            .await?;
        check_stop(&context)?;
        if snapshot.scope.session_id != owner.session_id || snapshot.scope.user_id != owner.user_id
        {
            return Err(rejected("后端工作区快照身份与当前原会话不符"));
        }
        let expected = if directory == "." {
            snapshot.workspace_root.clone()
        } else {
            snapshot.workspace_root.join(directory)
        };
        if snapshot.root != expected {
            return Err(rejected("后端工作区快照与所选目录不符"));
        }
        workspace_identity(&snapshot)?;
        Ok(snapshot)
    }
    /// 只冻结实际目录身份、选择器和运行策略；入口可以尚未创建，不冻结占位字节。
    pub async fn prepare_contract(
        &self,
        directory: &str,
        entry: &str,
        args: &[String],
        context: ToolExecutionContext,
    ) -> Result<TaskCompletionContract, ToolExecutionError> {
        validate_workspace_node_selector(directory, entry, args)
            .map_err(|_| rejected("工作区 Node 选择器无效"))?;
        let command_sha256 = self.runner.command_sha256_for(entry, args)?;
        let snapshot = self.snapshot(directory, context).await?;
        let contract = TaskCompletionContract::WorkspaceNodeTest {
            workspace_sha256: workspace_identity(&snapshot)?,
            directory: directory.into(),
            entry: entry.into(),
            args: args.to_vec(),
            runtime_sha256: self.runner.runtime_sha256().into(),
            command_sha256,
        };
        contract
            .validate()
            .map_err(|_| rejected("工作区 Node 合同无效"))?;
        Ok(contract)
    }
    /// 当前观察只重新读取原目录；无测试启动、无工具重放，也不补发预算。
    pub async fn observe_current(
        &self,
        contract: &TaskCompletionContract,
        owner: &TaskOwner,
        session: &ModelSession,
        checkpoint_sha256: &str,
        context: ToolExecutionContext,
    ) -> Result<WorkspaceObservation, ToolExecutionError> {
        contract
            .validate()
            .map_err(|_| rejected("工作区 Node 合同无效"))?;
        let (actual_owner, actual_session) = context_identity(&context)?;
        if owner != &actual_owner
            || session != &actual_session
            || checkpoint_sha256.len() != 64
            || !checkpoint_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(rejected("工作区收尾观察与原任务身份或检查点不符"));
        }
        let TaskCompletionContract::WorkspaceNodeTest {
            workspace_sha256,
            directory,
            entry,
            args,
            runtime_sha256,
            command_sha256,
        } = contract
        else {
            return Err(rejected("当前合同不是工作区 Node 测试"));
        };
        if runtime_sha256 != self.runner.runtime_sha256()
            || command_sha256 != &self.runner.command_sha256_for(entry, args)?
        {
            return Err(rejected("工作区 Node 原运行策略已变化"));
        }
        let snapshot = self.snapshot(directory, context.clone()).await?;
        let observed_workspace = workspace_identity(&snapshot)?;
        if &observed_workspace != workspace_sha256 {
            return Err(rejected("工作区 Node 原目录身份已变化"));
        }
        let bundle = bundle_from_snapshot(snapshot, entry, args)?;
        check_stop(&context)?;
        let observed_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| unavailable())?
            .as_millis()
            .try_into()
            .map_err(|_| unavailable())?;
        Ok(WorkspaceObservation {
            owner: owner.clone(),
            model_session: session.clone(),
            checkpoint_sha256: checkpoint_sha256.into(),
            workspace_sha256: observed_workspace,
            directory: directory.clone(),
            entry: entry.clone(),
            args: args.clone(),
            input_sha256: bundle.sha256().into(),
            observed_at_ms,
        })
    }
    async fn execute(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        let request =
            WorkspaceNodeRequest::parse(&call.arguments).map_err(|e| rejected(&e.message))?;
        let (owner, session) = context_identity(&context)?;
        if call.name != WORKSPACE_NODE_TEST_TOOL
            || context.call_id() != Some(call.id.as_str())
            || call.id.trim().is_empty()
            || call.id.len() > 256
            || call.id.chars().any(char::is_control)
        {
            return Err(rejected("工作区测试调用绑定无效"));
        }
        let key = invocation_id(&owner, &session, &call.id);
        {
            let mut ledger = self.ledger.lock().map_err(|_| unavailable())?;
            if ledger.admitted.len() >= MAX_INVOCATIONS || !ledger.admitted.insert(key.clone()) {
                return Err(rejected("工作区测试原调用已经准入或账本已满；不重放"));
            }
        }
        // 准入后即保留原调用；采集失败、取消或 future 丢弃都不退还重放资格。
        let snapshot = self.snapshot(&request.directory, context.clone()).await?;
        let workspace_sha256 = workspace_identity(&snapshot)?;
        let bundle = bundle_from_snapshot(snapshot, &request.entry, &request.args)?;
        let receipt = self
            .runner
            .run_precharged(&bundle, &execution_id(&session), &call.id, context)
            .await?;
        if receipt.owner().session_id != owner.session_id
            || receipt.owner().user_id != owner.user_id
            || receipt.execution_id() != execution_id(&session)
            || receipt.call_id() != call.id
            || receipt.input_sha256() != bundle.sha256()
            || receipt.runtime_sha256() != self.runner.runtime_sha256()
            || receipt.command_sha256() != self.runner.command_sha256(&bundle)?
        {
            return Err(unavailable());
        }
        let output = json!({"kind":"workspace_node_test","workspace_sha256":workspace_sha256,"directory":request.directory,"entry":request.entry,"args":request.args,"receipt":receipt});
        let result =
            ToolResult::success(call.id.clone(), output.clone()).map_err(|_| unavailable())?;
        let evidence = WorkspaceNodeEvidence {
            owner,
            model_session: session,
            call_id: call.id,
            arguments_sha256: task_strategy_digest(&call.arguments),
            tool_result_sha256: task_strategy_digest(&result),
            input_sha256: receipt.input_sha256().into(),
            workspace_sha256,
            directory: request.directory,
            entry: request.entry,
            args: request.args,
            runtime_sha256: receipt.runtime_sha256().into(),
            command_sha256: receipt.command_sha256().into(),
            outcome: match receipt.outcome() {
                FrozenNodeOutcome::Exited { exit_code, signal } => TaskFrozenNodeOutcome::Exited {
                    exit_code: *exit_code,
                    signal: *signal,
                },
                FrozenNodeOutcome::Cancelled => TaskFrozenNodeOutcome::Cancelled,
                FrozenNodeOutcome::Unknown => TaskFrozenNodeOutcome::Unknown,
            },
        };
        let mut ledger = self.ledger.lock().map_err(|_| unavailable())?;
        if !ledger.admitted.contains(&key) || ledger.receipts.contains_key(&key) {
            return Err(unavailable());
        }
        ledger
            .receipts
            .insert(key, LedgerReceipt { evidence, receipt });
        Ok(output)
    }
}
pub struct WorkspaceNodeTool(pub Arc<WorkspaceNodeAdapter>);
impl Tool for WorkspaceNodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: WORKSPACE_NODE_TEST_TOOL.into(),
            description: "采集当前身份工作区指定目录的完整输入，在宿主固定运行时执行纯 JS 测试；directory 默认 .，entry 必填，args 可选。不下载依赖；超过 64 文件或 8 MiB 拒绝且不裁剪。保留目录和执行位，管理元数据不进入沙箱；回执只证明实际冻结输入的测试结果。当前有效性由收尾只读核对，不要求最后一批重跑。未知、取消与原调用不自动重放。".into(),
            argument_schema: json!({"type":"object","properties":{"directory":{"type":"string","default":"."},"entry":{"type":"string"},"args":{"type":"array","items":{"type":"string"},"maxItems":16}},"required":["entry"],"additionalProperties":false}),
            required_permissions: vec![], concurrency: Some(ToolConcurrency::Serial { scope: "workspace_node_test".into() }),
        }
    }
    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        WorkspaceNodeRequest::parse(arguments).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(self.0.execute(call, context))
    }
}
impl WorkspaceNodeEvidenceSource for WorkspaceNodeAdapter {
    fn capture(
        &self,
        owner: &TaskOwner,
        frame: &TurnCheckpoint,
    ) -> TaskResult<Vec<WorkspaceNodeEvidence>> {
        validate_owner(owner)?;
        if frame.scope.session_id != owner.session_id || frame.scope.user_id != owner.user_id {
            return Err(TaskError::CorruptState);
        }
        let session = frame
            .model_session
            .as_ref()
            .filter(|session| session.turn_id().is_some())
            .ok_or(TaskError::CorruptState)?;
        session
            .validate_scope(Some(&frame.scope))
            .map_err(|_| TaskError::CorruptState)?;
        let ledger = self.ledger.lock().map_err(|_| TaskError::Storage)?;
        let mut evidence = Vec::new();
        for (index, message) in frame.messages.iter().enumerate() {
            for call in message
                .tool_calls
                .iter()
                .filter(|call| call.name == WORKSPACE_NODE_TEST_TOOL)
            {
                let Some(saved) = ledger
                    .receipts
                    .get(&invocation_id(owner, session, &call.id))
                else {
                    continue;
                };
                let Some(result) = frame.messages.get(index + 1).and_then(|message| {
                    message
                        .tool_results
                        .iter()
                        .find(|result| result.call_id == call.id)
                }) else {
                    continue;
                };
                if saved.evidence.owner != *owner
                    || saved.evidence.model_session.for_request(0) != session.for_request(0)
                    || saved.receipt.execution_id() != execution_id(&saved.evidence.model_session)
                    || saved.receipt.call_id() != call.id
                    || saved.evidence.arguments_sha256 != task_strategy_digest(&call.arguments)
                    || saved.evidence.tool_result_sha256 != task_strategy_digest(result)
                {
                    return Err(TaskError::CorruptState);
                }
                // 原执行回执仍绑定真实 request session；只在上面验证了完整同轮身份及
                // 原调用/结果后，映射为本检查点的执行会话，供持久验收器精确核对。
                let mut captured = saved.evidence.clone();
                captured.model_session = session.clone();
                evidence.push(captured);
            }
        }
        validate_workspace_node_evidence(owner, frame, &evidence)?;
        Ok(evidence)
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod tests;
