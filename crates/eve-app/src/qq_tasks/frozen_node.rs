//! 可信宿主注入的冻结 Node 测试组合：模型仅选择预注册 ID，见证只来自真实运行器。
//! 内存账本仅供本进程检查点采集，不是恢复队列；跨进程保全由任务/子代理日志负责。
use eve_llm_api::{
    LlmError, ModelSession, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolResult, ToolValidationError, TurnCheckpoint,
};
use eve_script_runtime::{
    FrozenNodeOutcome, FrozenNodeTestBundle, FrozenNodeTestReceipt, FrozenNodeTestRunner,
};
use eve_task_api::{
    FROZEN_NODE_TEST_TOOL, TaskCompletionContract, TaskCompletionEvidenceSource, TaskError,
    TaskFrozenNodeEvidence, TaskFrozenNodeOutcome, TaskOwner, TaskResult, task_strategy_digest,
    validate_frozen_node_evidence, validate_owner,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

const MAX_BINDINGS: usize = 64;
const MAX_INVOCATIONS: usize = 1024;

/// 只能由可信组合代码构造；原输入、运行时和命令摘要须在任何工具执行之前冻结。
pub struct FrozenNodeBinding {
    pub id: String,
    pub owner: TaskOwner,
    pub bundle: FrozenNodeTestBundle,
    pub contract: TaskCompletionContract,
}

struct LedgerReceipt {
    evidence: TaskFrozenNodeEvidence,
    // 真实类型无 Deserialize：绝不从模型输出或持久 JSON 重新赋予见证权威。
    receipt: FrozenNodeTestReceipt,
}
#[derive(Default)]
struct Ledger {
    admitted: BTreeSet<String>,
    receipts: BTreeMap<String, LedgerReceipt>,
}

/// 不在生产入口默认构造；调用方同时把 tool 和本对象作为 evidence source 注入同一宿主。
pub struct FrozenNodeAdapter {
    runner: FrozenNodeTestRunner,
    bindings: BTreeMap<String, FrozenNodeBinding>,
    ledger: Mutex<Ledger>,
}

fn rejected(message: &str) -> ToolExecutionError {
    ToolExecutionError::Rejected(message.into())
}
fn unavailable() -> ToolExecutionError {
    ToolExecutionError::Failed("冻结测试见证账本不可用；不宣称完成".into())
}
fn valid_bundle_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn execution_id(session: &ModelSession) -> String {
    task_strategy_digest(&("eve.frozen_node.execution.v1", session))
}
fn invocation_id(owner: &TaskOwner, session: &ModelSession, call_id: &str) -> String {
    task_strategy_digest(&("eve.frozen_node.call.v1", owner, session, call_id))
}
fn arguments_id(arguments: &Value) -> Result<&str, ToolValidationError> {
    let invalid = || ToolValidationError {
        message: "参数只能包含预注册的 bundle_id".into(),
    };
    let object = arguments.as_object().ok_or_else(invalid)?;
    let id = object
        .get("bundle_id")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if object.len() != 1 || !valid_bundle_id(id) {
        return Err(invalid());
    }
    Ok(id)
}

impl FrozenNodeAdapter {
    pub fn new(
        runner: FrozenNodeTestRunner,
        bindings: Vec<FrozenNodeBinding>,
    ) -> Result<Arc<Self>, LlmError> {
        let invalid = || LlmError::Configuration("冻结测试预注册输入或验收摘要不一致".into());
        if bindings.is_empty() || bindings.len() > MAX_BINDINGS {
            return Err(invalid());
        }
        let mut registered = BTreeMap::new();
        for binding in bindings {
            validate_owner(&binding.owner).map_err(|_| invalid())?;
            binding.contract.validate().map_err(|_| invalid())?;
            let TaskCompletionContract::FrozenNodeTest {
                input_sha256,
                runtime_sha256,
                command_sha256,
            } = &binding.contract
            else {
                return Err(invalid());
            };
            if !valid_bundle_id(&binding.id)
                || input_sha256 != binding.bundle.sha256()
                || runtime_sha256 != runner.runtime_sha256()
                || command_sha256
                    != &runner
                        .command_sha256(&binding.bundle)
                        .map_err(|_| invalid())?
                || registered.contains_key(&binding.id)
            {
                return Err(invalid());
            }
            registered.insert(binding.id.clone(), binding);
        }
        Ok(Arc::new(Self {
            runner,
            bindings: registered,
            ledger: Mutex::default(),
        }))
    }

    async fn execute(
        &self,
        call: ToolCall,
        context: ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        let bundle_id = arguments_id(&call.arguments).map_err(|e| rejected(&e.message))?;
        let owner = context
            .scope()
            .map(|scope| TaskOwner {
                session_id: scope.session_id.clone(),
                user_id: scope.user_id.clone(),
            })
            .ok_or_else(|| rejected("冻结测试缺少可信会话身份"))?;
        validate_owner(&owner).map_err(|_| rejected("冻结测试会话身份无效"))?;
        let session = context
            .model_session()
            .filter(|session| session.turn_id().is_some())
            .ok_or_else(|| rejected("冻结测试缺少原模型会话与轮次"))?
            .clone();
        session
            .validate_scope(context.scope())
            .map_err(|_| rejected("冻结测试模型会话与身份不符"))?;
        if call.name != FROZEN_NODE_TEST_TOOL
            || context.call_id() != Some(call.id.as_str())
            || call.id.trim().is_empty()
            || call.id.len() > 256
            || call.id.chars().any(char::is_control)
            || context.shared_steps().is_none()
        {
            return Err(rejected("冻结测试调用或原预算绑定无效"));
        }
        let binding = self
            .bindings
            .get(bundle_id)
            .filter(|binding| binding.owner == owner)
            .ok_or_else(|| rejected("冻结测试输入未为当前身份预注册"))?;
        let key = invocation_id(&owner, &session, &call.id);
        {
            let mut ledger = self.ledger.lock().map_err(|_| unavailable())?;
            if ledger.admitted.len() >= MAX_INVOCATIONS || !ledger.admitted.insert(key.clone()) {
                return Err(rejected("冻结测试原调用已经准入或账本已满；不重放"));
            }
        }
        // 先保留已准入身份；后端拒绝、取消、future 丢弃均不退还同调用重跑资格。
        let receipt = self
            .runner
            .run_precharged(&binding.bundle, &execution_id(&session), &call.id, context)
            .await?;
        if receipt.owner().session_id != owner.session_id
            || receipt.owner().user_id != owner.user_id
            || receipt.execution_id() != execution_id(&session)
            || receipt.call_id() != call.id
            || receipt.input_sha256() != binding.bundle.sha256()
            || receipt.runtime_sha256() != self.runner.runtime_sha256()
            || receipt.command_sha256() != self.runner.command_sha256(&binding.bundle)?
        {
            return Err(unavailable());
        }
        let output = json!({"kind":"frozen_node_test", "receipt":receipt});
        let result =
            ToolResult::success(call.id.clone(), output.clone()).map_err(|_| unavailable())?;
        let evidence = TaskFrozenNodeEvidence {
            owner,
            model_session: session,
            call_id: call.id,
            arguments_sha256: task_strategy_digest(&call.arguments),
            tool_result_sha256: task_strategy_digest(&result),
            input_sha256: receipt.input_sha256().into(),
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

pub struct FrozenNodeTool(pub Arc<FrozenNodeAdapter>);
impl Tool for FrozenNodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: FROZEN_NODE_TEST_TOOL.into(),
            description: "执行宿主预注册的完整冻结纯 JS 测试包；只接受 bundle_id，不读取指定源码路径，不下载依赖。回执只证明该冻结输入在固定运行时的真实结果，不证明活工作区或整体任务已完成。同一调用不重放；未知或取消不自动恢复，已知非零退出的后续处理仍遵守原合同与预算。".into(),
            argument_schema: json!({"type":"object","properties":{"bundle_id":{"type":"string","minLength":1,"maxLength":128}},"required":["bundle_id"],"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {scope:"frozen_node_test".into()}),
        }
    }
    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        arguments_id(arguments).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(self.0.execute(call, context))
    }
}

impl TaskCompletionEvidenceSource for FrozenNodeAdapter {
    fn capture(
        &self,
        owner: &TaskOwner,
        frame: &TurnCheckpoint,
    ) -> TaskResult<Vec<TaskFrozenNodeEvidence>> {
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
                .filter(|call| call.name == FROZEN_NODE_TEST_TOOL)
            {
                let key = invocation_id(owner, session, &call.id);
                let Some(saved) = ledger.receipts.get(&key) else {
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
                    || saved.evidence.model_session != *session
                    || saved.receipt.execution_id() != execution_id(session)
                    || saved.receipt.call_id() != call.id
                    || saved.evidence.arguments_sha256 != task_strategy_digest(&call.arguments)
                    || saved.evidence.tool_result_sha256 != task_strategy_digest(result)
                {
                    return Err(TaskError::CorruptState);
                }
                evidence.push(saved.evidence.clone());
            }
        }
        validate_frozen_node_evidence(owner, frame, &evidence)?;
        Ok(evidence)
    }
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod tests {
    use super::*;
    use eve_llm_api::{ChatMessage, ChatRole, ContextScope, SharedToolSteps};
    use eve_script_runtime::FrozenNodeRuntime;
    use std::{
        fs::{self, File},
        io::{Read, Seek, SeekFrom},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
    };

    fn owner() -> TaskOwner {
        TaskOwner {
            session_id: "adapter-session".into(),
            user_id: "adapter-owner".into(),
        }
    }
    fn scope() -> ContextScope {
        ContextScope {
            session_id: owner().session_id,
            user_id: owner().user_id,
        }
    }
    fn session() -> ModelSession {
        ModelSession::scoped("coding", &scope())
            .unwrap()
            .with_turn("original-turn")
            .unwrap()
    }
    fn context(id: &str, pool: &SharedToolSteps) -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(scope()))
            .with_model_session(session())
            .with_call_id(id.into())
            .with_shared_steps(pool.clone())
    }
    fn trusted_binary(name: &str) -> PathBuf {
        let path = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
            .unwrap_or_else(|| panic!("Linux 冻结进程验收缺少可信宿主工具：{name}"))
            .canonicalize()
            .expect("可信工具须能解析为真实绝对路径");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file() && metadata.permissions().mode() & 0o100 != 0);
        path
    }

    // 只解析已由宿主信任的 Node ELF，不执行待测 bundle，也不查询动态来源。
    // ldd 通常列出解释器，但单独读取 PT_INTERP 避免漏掉实际 exec 所需的绝对路径。
    fn elf_interpreter(binary: &Path) -> PathBuf {
        let mut file = File::open(binary).unwrap();
        let mut header = [0u8; 64];
        file.read_exact(&mut header).unwrap();
        assert_eq!(&header[..4], b"\x7fELF", "可信 Node 须为 ELF");
        assert_eq!(header[4], 2, "首版 fixture 只核对 ELF64");
        assert_eq!(header[5], 1, "首版 fixture 只核对小端 ELF64");
        let offset = u64::from_le_bytes(header[32..40].try_into().unwrap());
        let size = u16::from_le_bytes(header[54..56].try_into().unwrap()) as u64;
        let count = u16::from_le_bytes(header[56..58].try_into().unwrap()) as u64;
        assert!(size >= 56 && count > 0 && count <= 1024);
        for index in 0..count {
            file.seek(SeekFrom::Start(offset + index * size)).unwrap();
            let mut program = [0u8; 56];
            file.read_exact(&mut program).unwrap();
            if u32::from_le_bytes(program[..4].try_into().unwrap()) != 3 {
                continue;
            }
            let at = u64::from_le_bytes(program[8..16].try_into().unwrap());
            let bytes = u64::from_le_bytes(program[32..40].try_into().unwrap());
            assert!((2..=4096).contains(&bytes));
            file.seek(SeekFrom::Start(at)).unwrap();
            let mut interpreter = vec![0; bytes as usize];
            file.read_exact(&mut interpreter).unwrap();
            assert_eq!(interpreter.pop(), Some(0));
            let path = PathBuf::from(String::from_utf8(interpreter).unwrap());
            assert!(path.is_absolute());
            return path;
        }
        panic!("可信 Node 缺少 PT_INTERP；此 fixture 不支持静态运行时");
    }

    fn ldd_paths(ldd: &Path, trusted_elf: &Path) -> Vec<PathBuf> {
        assert_eq!(fs::canonicalize(trusted_elf).unwrap(), trusted_elf);
        let output = Command::new(ldd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C")
            .arg(trusted_elf)
            .output()
            .expect("可信 ELF 依赖查询须能启动");
        assert!(
            output.status.success(),
            "可信 ELF 依赖查询失败：{}，{}",
            trusted_elf.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("not found"), "可信 Node 依赖不完整：{text}");
        if text.trim() == "statically linked" {
            return vec![];
        }
        let mut paths = Vec::new();
        for line in text.lines() {
            let words: Vec<_> = line.split_whitespace().collect();
            if words.is_empty() || words[0].starts_with("linux-vdso") {
                continue;
            }
            let name = words
                .iter()
                .position(|word| *word == "=>")
                .and_then(|index| words.get(index + 1))
                .copied()
                .unwrap_or(words[0]);
            assert!(name.starts_with('/'), "未核对的 ldd 输出：{line}");
            let path = PathBuf::from(name);
            assert!(path.is_file(), "可信运行时依赖缺失：{}", path.display());
            paths.push(path);
        }
        paths
    }

    fn copy_regular(source: &Path, destination: &Path, root: &Path) {
        assert_eq!(source.canonicalize().unwrap(), source);
        assert!(fs::symlink_metadata(source).unwrap().is_file());
        let parent = destination.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        for directory in parent
            .ancestors()
            .take_while(|directory| *directory != root)
        {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::copy(source, destination).unwrap();
        let executable = fs::metadata(source).unwrap().permissions().mode() & 0o111;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o644 | executable)).unwrap();
        assert!(fs::symlink_metadata(destination).unwrap().is_file());
    }

    fn real_runner() -> FrozenNodeTestRunner {
        let node = trusted_binary("node");
        let ldd = trusted_binary("ldd");
        let interpreter = elf_interpreter(&node);
        let loader = interpreter.canonicalize().unwrap();
        let mut libraries = BTreeMap::from([(interpreter, loader.clone())]);
        let mut pending = vec![node.clone()];
        let mut seen = BTreeSet::new();
        while let Some(elf) = pending.pop() {
            if !seen.insert(elf.clone()) {
                continue;
            }
            for lookup in ldd_paths(&ldd, &elf) {
                let canonical = lookup.canonicalize().unwrap();
                assert!(fs::symlink_metadata(&canonical).unwrap().is_file());
                libraries.insert(lookup, canonical.clone());
                if canonical != loader {
                    pending.push(canonical);
                }
            }
        }
        let root = tempfile::tempdir().unwrap();
        copy_regular(&node, &root.path().join("runtime/node"), root.path());
        for (lookup, canonical) in libraries {
            copy_regular(
                &canonical,
                &root.path().join(lookup.strip_prefix("/").unwrap()),
                root.path(),
            );
        }
        let ctx = ToolExecutionContext::new();
        let digest = FrozenNodeRuntime::bundle_sha256(root.path(), "runtime/node", &ctx).unwrap();
        let runtime =
            FrozenNodeRuntime::prepare(root.path(), "runtime/node", &digest, &ctx).unwrap();
        FrozenNodeTestRunner::new(runtime, launcher()).unwrap()
    }
    fn launcher() -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bwrap"))
            .find(|path| path.is_file())
            .expect("冻结适配器验收需要真实 bubblewrap，不能降级宿主执行")
            .canonicalize()
            .unwrap()
    }
    /// 故意没有合法 Node，真实隔离启动必须留下 Unknown，绝不能伪称测试通过。
    fn unavailable_runner() -> FrozenNodeTestRunner {
        let root = tempfile::tempdir().unwrap();
        let node = root.path().join("node");
        std::fs::write(&node, b"intentionally-not-an-executable").unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ctx = ToolExecutionContext::new();
        let hash = FrozenNodeRuntime::bundle_sha256(root.path(), "node", &ctx).unwrap();
        let runtime = FrozenNodeRuntime::prepare(root.path(), "node", &hash, &ctx).unwrap();
        FrozenNodeTestRunner::new(runtime, launcher()).unwrap()
    }
    fn binding(runner: &FrozenNodeTestRunner) -> FrozenNodeBinding {
        let bundle = FrozenNodeTestBundle::new(
            BTreeMap::from([
                ("entry.cjs".into(), b"require('node:assert/strict').equal(require('./math.cjs')(20,22),42); console.log('frozen assertions passed');".to_vec()),
                ("math.cjs".into(), b"module.exports=(a,b)=>a+b;".to_vec()),
            ]),
            "entry.cjs".into(),
            vec![],
        )
        .unwrap();
        let contract = TaskCompletionContract::FrozenNodeTest {
            input_sha256: bundle.sha256().into(),
            runtime_sha256: runner.runtime_sha256().into(),
            command_sha256: runner.command_sha256(&bundle).unwrap(),
        };
        FrozenNodeBinding {
            id: "bundle-opaque".into(),
            owner: owner(),
            bundle,
            contract,
        }
    }
    fn call() -> ToolCall {
        ToolCall {
            id: "real-call".into(),
            name: FROZEN_NODE_TEST_TOOL.into(),
            arguments: json!({"bundle_id":"bundle-opaque"}),
        }
    }
    fn frame(call: ToolCall, output: Value) -> TurnCheckpoint {
        let id = call.id.clone();
        TurnCheckpoint {
            version: 1,
            scope: scope(),
            model_session: Some(session()),
            pending_model_request: None,
            pending_request_fingerprint: None,
            messages: vec![
                ChatMessage::text(ChatRole::User, "运行预先冻结的测试"),
                ChatMessage::assistant_tool_calls(vec![call]).unwrap(),
                ChatMessage::tool_results(vec![ToolResult::success(id, output).unwrap()]).unwrap(),
            ],
            pending_calls: vec![],
            used_tool_calls: 1,
            delegated_tool_calls: 0,
            delegated_model_requests: 0,
            provider_requests: 1,
            tool_batches: 1,
            remaining: None,
            continuation_checks: 0,
            closing: false,
            result: None,
        }
    }
    #[tokio::test]
    async fn real_unavailable_runner_preserves_unknown_and_exact_receipt_without_budget_reset_or_replay()
     {
        let runner = unavailable_runner();
        let binding = binding(&runner);
        let adapter = FrozenNodeAdapter::new(runner, vec![binding]).unwrap();
        let tool = FrozenNodeTool(adapter.clone());
        let pool = SharedToolSteps::new(10).unwrap();
        pool.reserve(4).unwrap();
        pool.record_model_request().unwrap();
        let charged = pool.usage().unwrap();
        let output = tool
            .execute(call(), context("real-call", &pool))
            .await
            .unwrap();
        assert_eq!(output["receipt"]["outcome"]["status"], "unknown");
        assert_eq!(pool.usage().unwrap(), charged);
        let frame = frame(call(), output);
        frame.validate(10).unwrap();
        let first = adapter.capture(&owner(), &frame).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].outcome, TaskFrozenNodeOutcome::Unknown);
        assert_eq!(
            first,
            adapter.capture(&owner(), &frame).unwrap(),
            "capture必须只读且不drain"
        );
        assert!(
            tool.execute(call(), context("real-call", &pool))
                .await
                .is_err()
        );
        assert_eq!(pool.usage().unwrap(), charged);
        assert_eq!(first, adapter.capture(&owner(), &frame).unwrap());
        let mut changed = frame.clone();
        changed.messages[1].tool_calls[0].arguments = json!({"bundle_id":"another"});
        assert!(adapter.capture(&owner(), &changed).is_err());
        let mut changed = frame.clone();
        changed.messages[2].tool_results[0] =
            ToolResult::success("real-call", json!({"exit_code":0,"status":"verified"})).unwrap();
        assert!(adapter.capture(&owner(), &changed).is_err());
        let mut other_turn = frame.clone();
        other_turn.model_session = Some(session().with_turn("other-turn").unwrap());
        assert!(adapter.capture(&owner(), &other_turn).unwrap().is_empty());
        assert_eq!(first, adapter.capture(&owner(), &frame).unwrap());
    }
    #[tokio::test]
    async fn unregistered_or_unbound_calls_and_fabricated_json_never_create_typed_evidence() {
        let runner = unavailable_runner();
        let binding = binding(&runner);
        let adapter = FrozenNodeAdapter::new(runner, vec![binding]).unwrap();
        let tool = FrozenNodeTool(adapter.clone());
        let pool = SharedToolSteps::new(10).unwrap();
        for args in [
            json!({"bundle_id":"../source"}),
            json!({"bundle_id":"bundle-opaque","command":"node"}),
            json!({"path":"entry.cjs"}),
        ] {
            assert!(tool.validate_arguments(&args).is_err());
        }
        assert!(
            tool.execute(call(), ToolExecutionContext::new())
                .await
                .is_err()
        );
        assert!(
            tool.execute(call(), context("wrong-call", &pool))
                .await
                .is_err()
        );
        let mut other = call();
        other.arguments = json!({"bundle_id":"missing"});
        assert!(
            tool.execute(other, context("real-call", &pool))
                .await
                .is_err()
        );
        let mut other = call();
        other.name = "echo".into();
        assert!(
            tool.execute(other, context("real-call", &pool))
                .await
                .is_err()
        );
        let foreign_scope = ContextScope {
            user_id: "stranger".into(),
            ..scope()
        };
        let foreign = ToolExecutionContext::new()
            .with_scope(Some(foreign_scope.clone()))
            .with_model_session(
                ModelSession::scoped("coding", &foreign_scope)
                    .unwrap()
                    .with_turn("original-turn")
                    .unwrap(),
            )
            .with_call_id("real-call".into())
            .with_shared_steps(pool);
        assert!(tool.execute(call(), foreign).await.is_err());
        let fake = frame(
            call(),
            json!({"kind":"frozen_node_test","receipt":{"outcome":{"status":"exited","exit_code":0}}}),
        );
        assert!(adapter.capture(&owner(), &fake).unwrap().is_empty());
        assert!(adapter.ledger.lock().unwrap().admitted.is_empty());
    }
    #[test]
    fn registration_rejects_post_hoc_or_mismatched_contract() {
        let runner = unavailable_runner();
        let mut binding = binding(&runner);
        binding.contract = TaskCompletionContract::Unspecified;
        assert!(FrozenNodeAdapter::new(runner, vec![binding]).is_err());
        let runner = unavailable_runner();
        let mut binding = self::binding(&runner);
        let TaskCompletionContract::FrozenNodeTest { input_sha256, .. } = &mut binding.contract
        else {
            unreachable!()
        };
        *input_sha256 = "0".repeat(64);
        assert!(FrozenNodeAdapter::new(runner, vec![binding]).is_err());
    }
    #[tokio::test]
    async fn real_node_success_is_checkpointed_finished_and_reopened_without_reexecuting() {
        use crate::qq_tasks::{
            WorkerRun,
            subagent_journal::{FrozenRun, RunStorage, RunStore, SqliteRunStorage},
        };
        use eve_llm_api::TurnJournal;
        use eve_task_api::{TaskCompletionAssessment, TaskEnd, TaskUsage};
        let runner = real_runner();
        let binding = binding(&runner);
        let contract = binding.contract.clone();
        let adapter = FrozenNodeAdapter::new(runner, vec![binding]).unwrap();
        let tool = FrozenNodeTool(adapter.clone());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runs.sqlite3");
        let backend = Arc::new(SqliteRunStorage::open(&path).unwrap());
        let store = RunStore::open_with_completion_evidence(backend.clone(), Some(adapter.clone()));
        let frozen = FrozenRun {
            owner: owner(),
            model_session: session(),
            parent_call_id: "parent-test".into(),
            profile: crate::qq_subagents::builtin("coding").unwrap(),
            instructions: "验证已冻结纯 JS 输入".into(),
            context: None,
            review_messages: vec!["验证已冻结纯 JS 输入".into()],
            verification: contract,
            steps: 8,
            deadline_at_ms: None,
        };
        let journal = store.begin(frozen.clone()).unwrap();
        let id = journal.id.clone();
        let pool = SharedToolSteps::new(10).unwrap();
        pool.reserve(4).unwrap();
        pool.record_model_request().unwrap();
        let before = pool.usage().unwrap();
        let output = tool
            .execute(call(), context("real-call", &pool))
            .await
            .unwrap();
        assert_eq!(
            output["receipt"]["outcome"]["status"], "exited",
            "真实 Node 隔离运行必须成功，不能把缺环境当作通过"
        );
        assert_eq!(output["receipt"]["outcome"]["exit_code"], 0);
        assert!(
            output["receipt"]["stdout"]
                .as_str()
                .unwrap()
                .contains("frozen assertions passed")
        );
        let mut frame = frame(call(), output);
        let evidence = adapter.capture(&owner(), &frame).unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].outcome,
            TaskFrozenNodeOutcome::Exited {
                exit_code: Some(0),
                signal: None
            }
        );
        journal.save(&frame).unwrap();
        frame.messages.push(
            ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "progress".into(),
                name: "task_progress".into(),
                arguments: json!({"status":"completed"}),
            }])
            .unwrap(),
        );
        frame.messages.push(
            ChatMessage::tool_results(vec![
                ToolResult::success("progress", json!({"status":"completed"})).unwrap(),
            ])
            .unwrap(),
        );
        frame.used_tool_calls += 1;
        frame.tool_batches += 1;
        frame.provider_requests += 1;
        frame.result = Some("冻结输入测试通过；未验证活源码".into());
        journal.save(&frame).unwrap();
        let mut run = WorkerRun {
            end: TaskEnd::Completed {
                result: frame.result.clone().unwrap(),
            },
            usage: TaskUsage::default(),
            verification: None,
            retryable: false,
        };
        journal.finish(&mut run).unwrap();
        assert!(matches!(run.end, TaskEnd::Completed { .. }));
        assert_eq!(run.verification, Some(TaskCompletionAssessment::Verified));
        assert_eq!(pool.usage().unwrap(), before);
        let raw = backend.read(&id).unwrap().unwrap();
        let state: Value = serde_json::from_slice(&raw.1).unwrap();
        assert_eq!(
            state["frozen_node_evidence"],
            serde_json::to_value(&evidence).unwrap()
        );
        assert_eq!(state["checkpoint"], serde_json::to_value(&frame).unwrap());
        drop(journal);
        drop(store);
        drop(backend);
        drop(tool);
        drop(adapter);
        let backend = Arc::new(SqliteRunStorage::open(&path).unwrap());
        let reopened = RunStore::open(backend.clone());
        let view = reopened.evidence_view(&owner(), &id).unwrap();
        assert_eq!(view["runs"][0]["verification"], "verified");
        assert_eq!(view["runs"][0]["current_artifact_validity"], "not_checked");
        assert_eq!(backend.read(&id).unwrap().unwrap(), raw);
        assert!(reopened.begin(frozen).is_err());
        assert_eq!(pool.usage().unwrap(), before);
    }
}
