//! 最小纯脚本失败现场。只读原版本，隔离临时状态、无宿主工具和网络；不重试原调用。
use eve_llm_api::{
    ContextScope, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolValidationError,
};
use eve_plugin_api::{PluginId, StateStore};
use eve_script_runtime::{
    AuthorizedToolDispatch, Bubblewrap, Receipt, ReceiptStatus, ScriptRuntime, ScriptStore,
    VersionRef,
};
use eve_task_api::{TaskAdmin, TaskOwner};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, ToolExecutionError>;
const KEY: &str = "pure-script-failures.v1";
const MAX_BYTES: usize = 512 * 1024;
const RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
fn reject(e: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Rejected(e.to_string())
}
fn digest(v: &impl Serialize) -> Result<String> {
    Ok(ring::digest::digest(
        &ring::digest::SHA256,
        &serde_json::to_vec(v).map_err(reject)?,
    )
    .as_ref()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect())
}
/// 仅允许无文本的有界测试输入，拒绝凭据、任意日志和会话内容复制。
fn safe_input(v: &Value, depth: usize) -> bool {
    if depth > 8 {
        return false;
    }
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(_) => false,
        Value::Array(a) => a.len() <= 64 && a.iter().all(|v| safe_input(v, depth + 1)),
        Value::Object(o) => {
            o.len() <= 64
                && o.iter().all(|(k, v)| {
                    k.len() <= 40
                        && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        && !["secret", "token", "password", "credential", "key", "auth"]
                            .iter()
                            .any(|s| k.to_ascii_lowercase().contains(s))
                        && safe_input(v, depth + 1)
                })
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scene {
    schema: u32,
    owner: ContextScope,
    version: VersionRef,
    source_digest: String,
    receipt_digest: String,
    call_id: String,
    arguments: Value,
    task_id: Option<String>,
    created_at_ms: u64,
    expires_at_ms: u64,
    expected_failure: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    id: String,
    scene: Scene,
    replay: Option<Report>,
    #[serde(default)]
    baseline_reproduced: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    version: VersionRef,
    status: ReceiptStatus,
    fingerprint: String,
    reproduced: bool,
    candidate: bool,
    result_digest: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Audit {
    bundle_id: String,
    action: String,
    at_ms: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    bundles: Vec<Bundle>,
    audit: Vec<Audit>,
}

struct Service {
    scripts: Arc<ScriptStore>,
    sandbox: Bubblewrap,
    state: Arc<dyn StateStore>,
    tasks: Arc<dyn TaskAdmin>,
    lock: Mutex<()>,
}
impl Service {
    fn namespace() -> Result<PluginId> {
        PluginId::new("eve.failure-replay").map_err(reject)
    }
    fn load(&self) -> Result<Ledger> {
        let Some(bytes) = self.state.get(&Self::namespace()?, KEY).map_err(reject)? else {
            return Ok(Ledger::default());
        };
        if bytes.len() > MAX_BYTES {
            return Err(reject("失败包账本超限，保留原文件"));
        }
        let ledger: Ledger =
            serde_json::from_slice(&bytes).map_err(|_| reject("失败包账本损坏，未清空"))?;
        if ledger.bundles.len() > 32 || ledger.audit.len() > 256 {
            return Err(reject("失败包账本容量无效"));
        }
        let mut ids = std::collections::HashSet::new();
        for b in &ledger.bundles {
            if b.scene.schema != 1
                || b.id != digest(&b.scene)?
                || !ids.insert(&b.id)
                || !safe_input(&b.scene.arguments, 0)
                || b.scene.expires_at_ms != b.scene.created_at_ms.saturating_add(RETENTION_MS)
            {
                return Err(reject("失败包摘要/schema无效，未清空"));
            }
        }
        Ok(ledger)
    }
    fn save(&self, ledger: &Ledger) -> Result<()> {
        let bytes = serde_json::to_vec(ledger).map_err(reject)?;
        if bytes.len() > MAX_BYTES {
            return Err(reject("失败包容量已满"));
        }
        self.state
            .set(&Self::namespace()?, KEY.into(), bytes)
            .map_err(reject)
    }
    fn audit(ledger: &mut Ledger, id: &str, action: &str, now: u64) -> Result<()> {
        if ledger.audit.len() == 256 {
            return Err(reject("失败包审计容量已满；未静默覆盖历史"));
        }
        ledger.audit.push(Audit {
            bundle_id: id.into(),
            action: action.into(),
            at_ms: now,
        });
        Ok(())
    }
    fn pure_version(
        &self,
        scope: &ContextScope,
        reference: &VersionRef,
    ) -> Result<eve_script_runtime::PublishedVersion> {
        let version = self.scripts.version(scope, reference)?;
        if !matches!(
            version.spec.language,
            eve_script_runtime::Language::Python | eve_script_runtime::Language::Javascript
        ) || !version.spec.tools.is_empty()
            || !version.spec.dependencies.is_empty()
            || !version.spec.state_schema.fields.is_empty()
            || version.spec.initial_state != json!({})
        {
            return Err(reject(
                "unsupported：v1仅支持无工具、无依赖、空状态纯脚本；不复制外部现场",
            ));
        }
        Ok(version)
    }
    fn capture(&self, scope: &ContextScope, a: &Args, now: u64) -> Result<Value> {
        let call_id = a
            .call_id
            .as_deref()
            .ok_or_else(|| reject("capture需要call_id"))?;
        let arguments = a
            .arguments
            .clone()
            .ok_or_else(|| reject("capture需要原arguments"))?;
        if !safe_input(&arguments, 0)
            || serde_json::to_vec(&arguments).map_err(reject)?.len() > 8192
        {
            return Err(reject("输入不在安全数值fixture子集中"));
        }
        let receipt = self.scripts.receipt(scope, call_id)?;
        validate_receipt(&receipt)?;
        if receipt.request_digest != digest(&arguments)? {
            return Err(reject("输入与原回执摘要不一致"));
        }
        let version = self.pure_version(scope, &receipt.version)?;
        if let Some(task_id) = &a.task_id {
            let tasks = self.tasks.snapshot().map_err(reject)?;
            let task = tasks
                .tasks
                .iter()
                .find(|t| {
                    &t.id == task_id
                        && t.owner.session_id == scope.session_id
                        && t.owner.user_id == scope.user_id
                })
                .ok_or_else(|| reject("原范围没有此任务"))?;
            let checkpoint = task
                .execution
                .as_ref()
                .and_then(|e| e.checkpoint.as_ref())
                .ok_or_else(|| reject("原任务没有检查点"))?;
            if !checkpoint
                .messages
                .iter()
                .flat_map(|m| &m.tool_calls)
                .chain(&checkpoint.pending_calls)
                .any(|c| {
                    c.id == call_id
                        && c.name == "run_script_plugin"
                        && c.arguments.get("version")
                            == serde_json::to_value(&receipt.version).ok().as_ref()
                        && c.arguments.get("arguments") == Some(&arguments)
                })
            {
                return Err(reject("原任务没有匹配的脚本调用证据"));
            }
        }
        let scene = Scene {
            schema: 1,
            owner: scope.clone(),
            version: receipt.version.clone(),
            source_digest: digest(&version.spec)?,
            receipt_digest: digest(&receipt)?,
            call_id: call_id.into(),
            arguments,
            task_id: a.task_id.clone(),
            created_at_ms: now,
            expires_at_ms: now.saturating_add(RETENTION_MS),
            expected_failure: fingerprint(&receipt)?,
        };
        let id = digest(&scene)?;
        let _guard = self.lock.lock().map_err(reject)?;
        let mut ledger = self.load()?;
        // 明确七日保留期，只回收过期包；审计不静默丢弃。
        ledger.bundles.retain(|b| b.scene.expires_at_ms > now);
        if ledger.bundles.len() >= 32 {
            return Err(reject("失败包已满"));
        }
        Self::audit(&mut ledger, &id, "capture", now)?;
        let bundle = Bundle {
            id: id.clone(),
            scene,
            replay: None,
            baseline_reproduced: false,
        };
        let result = summary(&bundle);
        ledger.bundles.push(bundle);
        self.save(&ledger)?;
        Ok(result)
    }
    fn read(&self, scope: &ContextScope, id: &str, action: &str, now: u64) -> Result<Bundle> {
        let _guard = self.lock.lock().map_err(reject)?;
        let mut ledger = self.load()?;
        let bundle = ledger
            .bundles
            .iter()
            .find(|b| b.id == id && &b.scene.owner == scope && b.scene.expires_at_ms > now)
            .cloned()
            .ok_or_else(|| reject("本范围没有有效失败包"))?;
        Self::audit(&mut ledger, id, action, now)?;
        self.save(&ledger)?;
        Ok(bundle)
    }
    async fn replay(
        &self,
        bundle: &Bundle,
        candidate: Option<&VersionRef>,
        context: ToolExecutionContext,
    ) -> Result<Value> {
        if candidate.is_some() {
            let _guard = self.lock.lock().map_err(reject)?;
            if !self
                .load()?
                .bundles
                .iter()
                .any(|b| b.id == bundle.id && b.baseline_reproduced)
            {
                return Err(reject(
                    "先实际重放原失败并取得相同可观察指纹，再验证修复候选",
                ));
            }
        }
        let original = self.pure_version(&bundle.scene.owner, &bundle.scene.version)?;
        if digest(&original.spec)? != bundle.scene.source_digest {
            return Err(reject("源版本指纹不匹配"));
        }
        let reference = candidate.unwrap_or(&bundle.scene.version);
        let version = self.pure_version(&bundle.scene.owner, reference)?;
        if reference.script != bundle.scene.version.script {
            return Err(reject("修复候选必须属于同一脚本"));
        }
        // 临时目录只含本版本和无文本fixture。没有原状态、宿主文件、网络或可调用外部工具。
        let temporary = tempfile::tempdir().map_err(reject)?;
        let store = ScriptStore::open(temporary.path().join("state"))?;
        let copied = store.publish(&bundle.scene.owner, version.spec)?;
        store.activate(&bundle.scene.owner, &copied.reference, None)?;
        let runtime = ScriptRuntime::new(
            store,
            Arc::new(DenyTools),
            self.sandbox.clone(),
            temporary.path().join("artifacts"),
        );
        let bound = runtime.bind(&bundle.scene.owner, &copied.reference)?;
        let receipt = runtime
            .run(
                &bound,
                "isolated-replay",
                bundle.scene.arguments.clone(),
                context,
            )
            .await?;
        let fingerprint = fingerprint(&receipt)?;
        let report = Report {
            version: reference.clone(),
            status: receipt.status.clone(),
            reproduced: candidate.is_none() && fingerprint == bundle.scene.expected_failure,
            fingerprint,
            candidate: candidate.is_some(),
            result_digest: receipt.result.as_ref().map(digest).transpose()?,
        };
        let _guard = self.lock.lock().map_err(reject)?;
        let mut ledger = self.load()?;
        let saved = ledger
            .bundles
            .iter_mut()
            .find(|b| b.id == bundle.id)
            .ok_or_else(|| reject("失败包已不可用"))?;
        if candidate.is_none() {
            saved.baseline_reproduced = report.reproduced;
        }
        saved.replay = Some(report.clone());
        Self::audit(
            &mut ledger,
            &bundle.id,
            "replay_completed",
            crate::qq_tasks::now_ms(),
        )?;
        self.save(&ledger)?;
        Ok(
            json!({"simulation_only":true,"real_delivery":false,"report":report,"candidate_activated":false,"note":"真实隔离执行；修复候选通过只表示此fixture通过，不等于完整业务验收。原脚本未切换。"}),
        )
    }
}
fn unknown_effect(value: &Value, depth: usize) -> bool {
    if depth > 32 {
        return true;
    }
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            (matches!(
                key.as_str(),
                "status" | "state" | "delivery" | "commit" | "execution"
            ) && value
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("unknown")))
                || unknown_effect(value, depth + 1)
        }),
        Value::Array(values) => values.iter().any(|v| unknown_effect(v, depth + 1)),
        _ => false,
    }
}
fn validate_receipt(receipt: &Receipt) -> Result<()> {
    if !matches!(
        receipt.status,
        ReceiptStatus::Failed | ReceiptStatus::Rejected
    ) || !receipt.evidence.is_empty()
    {
        return Err(reject(
            "只采集无外部效果的明确失败；Unknown/取消/进行中不能重放",
        ));
    }
    if receipt.failure_diagnostic.is_none() {
        return Err(reject("unsupported：旧回执或此阶段缺少可比较的进程诊断"));
    }
    Ok(())
}
fn fingerprint(r: &Receipt) -> Result<String> {
    // 完整结构指纹不包含原调用ID、结果正文、临时路径或可信授权。
    digest(&(r.status.clone(), &r.failure_diagnostic, &r.evidence))
}
fn summary(b: &Bundle) -> Value {
    json!({"bundle_id":b.id,"schema":b.scene.schema,"version":b.scene.version,"source_digest":b.scene.source_digest,"receipt_digest":b.scene.receipt_digest,"input_digest":digest(&b.scene.arguments).ok(),"expires_at_ms":b.scene.expires_at_ms,"task_id":b.scene.task_id,"last_replay":b.replay,"mode":"isolated_pure_script","export_content":"metadata_only","limitations":"仅无文本fixture、空状态、无依赖/外部工具；源代码保留原不可变仓库，不导出源代码/聊天/凭据。"})
}
struct DenyTools;
impl AuthorizedToolDispatch for DenyTools {
    fn preflight_tool(&self, _: &str, _: &ToolExecutionContext) -> Result<()> {
        Err(reject("隔离复现禁止宿主工具"))
    }
    fn preflight_call(&self, _: &ToolCall, _: &ToolExecutionContext) -> Result<()> {
        Err(reject("隔离复现禁止宿主工具"))
    }
    fn dispatch(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async { Err(reject("隔离复现禁止宿主工具")) })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    bundle_id: Option<String>,
    call_id: Option<String>,
    arguments: Option<Value>,
    task_id: Option<String>,
    candidate: Option<VersionRef>,
}
struct ReplayTool(Arc<Service>);
impl Tool for ReplayTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition{name:"manage_failure_reproduction".into(),description:"采集纯脚本明确失败的最小现场，inspect/export_summary安全摘要，replay真实隔离复现，verify_candidate隔离验证候选。resume_original只把诊断附回原Blocked任务，共用原预算/期限；不重试未知副作用、不自动切换版本。capture需原call_id及完全匹配的数值fixture arguments，可绑定有调用证据的task_id。其他现场明确unsupported。".into(),argument_schema:json!({"type":"object","properties":{"action":{"enum":["capture","inspect","export_summary","replay","verify_candidate","resume_original"]},"bundle_id":{"type":"string"},"call_id":{"type":"string"},"arguments":{},"task_id":{"type":"string"},"candidate":{"type":"object"}},"required":["action"],"additionalProperties":false}),required_permissions:vec![],concurrency:Some(ToolConcurrency::Serial { scope: "failure-replay".into() })}
    }
    fn validate_arguments(&self, value: &Value) -> std::result::Result<(), ToolValidationError> {
        let invalid = || ToolValidationError {
            message: "失败复现参数无效".into(),
        };
        if value.to_string().len() > 16384 {
            return Err(invalid());
        }
        let a: Args = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
        if !matches!(
            a.action.as_str(),
            "capture"
                | "inspect"
                | "export_summary"
                | "replay"
                | "verify_candidate"
                | "resume_original"
        ) {
            return Err(invalid());
        }
        Ok(())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments).map_err(reject)?;
            if context.is_cancel_requested() {
                return Err(reject("已取消"));
            }
            let scope = context
                .scope()
                .cloned()
                .ok_or_else(|| reject("需要宿主绑定真实会话"))?;
            let a: Args = serde_json::from_value(call.arguments).map_err(reject)?;
            let now = crate::qq_tasks::now_ms();
            if a.action == "capture" {
                return self.0.capture(&scope, &a, now);
            }
            let id = a
                .bundle_id
                .as_deref()
                .ok_or_else(|| reject("需要bundle_id"))?;
            let bundle = self.0.read(&scope, id, &a.action, now)?;
            match a.action.as_str() {
                "inspect" | "export_summary" => Ok(summary(&bundle)),
                "replay" => self.0.replay(&bundle, None, context).await,
                "verify_candidate" => {
                    self.0
                        .replay(
                            &bundle,
                            Some(
                                a.candidate
                                    .as_ref()
                                    .ok_or_else(|| reject("需要candidate不可变版本"))?,
                            ),
                            context,
                        )
                        .await
                }
                "resume_original" => {
                    if context.is_delegated() {
                        return Err(reject("委派执行不能通过修复接口接续其他任务"));
                    }
                    let report = bundle
                        .replay
                        .as_ref()
                        .filter(|r| r.candidate && r.status == ReceiptStatus::Succeeded)
                        .ok_or_else(|| reject("必须先隔离验证修复候选"))?;
                    let task_id = bundle
                        .scene
                        .task_id
                        .as_ref()
                        .ok_or_else(|| reject("此包未绑定原任务"))?;
                    let snapshot = self.0.tasks.snapshot().map_err(reject)?;
                    let original = snapshot
                        .tasks
                        .iter()
                        .find(|t| {
                            &t.id == task_id
                                && t.owner.session_id == scope.session_id
                                && t.owner.user_id == scope.user_id
                        })
                        .ok_or_else(|| reject("原范围没有此任务"))?;
                    if original.notice.as_ref().is_some_and(|n| n.status==eve_task_api::NoticeStatus::Unknown)
                        || original.execution.as_ref().and_then(|e|e.checkpoint.as_ref()).is_none_or(|c| c.messages.iter().flat_map(|m|&m.tool_results).any(|r|matches!(&r.output,eve_llm_api::ToolOutput::Success(v) if unknown_effect(v,0)))) {
                        return Err(reject("原任务存在未知效果或缺少检查点，不能通过修复入口接续"));
                    }
                    let evidence = format!(
                        "隔离纯脚本fixture候选验证成功，仅为诊断不是授权。包{}；候选摘要{}；没有切换原版本/交付真实业务，继续原任务按原授权审查候选。",
                        bundle.id, report.version.digest
                    );
                    let task = self
                        .0
                        .tasks
                        .resume_blocked(
                            &TaskOwner {
                                session_id: scope.session_id,
                                user_id: scope.user_id,
                            },
                            task_id,
                            &evidence,
                            now,
                        )
                        .map_err(reject)?;
                    Ok(
                        json!({"task_id":task.id,"same_task":true,"new_budget":false,"candidate_activated":false}),
                    )
                }
                _ => Err(reject("不支持的操作")),
            }
        })
    }
}
pub(crate) fn tools(
    scripts: Arc<ScriptStore>,
    sandbox: Bubblewrap,
    state: Arc<dyn StateStore>,
    tasks: Arc<dyn TaskAdmin>,
) -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(ReplayTool(Arc::new(Service {
        scripts,
        sandbox,
        state,
        tasks,
        lock: Mutex::new(()),
    })))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
    use eve_script_runtime::{Language, ScriptSpec, StateSchema};
    use std::collections::BTreeMap;
    fn scope() -> ContextScope {
        ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        }
    }
    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(scope()))
            .with_shared_steps(eve_llm_api::SharedToolSteps::new(20).unwrap())
    }
    fn spec(source: &str) -> ScriptSpec {
        ScriptSpec {
            name: "replay-demo".into(),
            language: Language::Python,
            source: source.into(),
            dependencies: BTreeMap::new(),
            tools: vec![],
            state_schema: StateSchema {
                version: 1,
                fields: BTreeMap::new(),
            },
            initial_state: json!({}),
        }
    }
    fn sandbox() -> Bubblewrap {
        fn executable(name: &str) -> std::path::PathBuf {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join(name))
                .find(|p| p.is_file())
                .unwrap()
                .canonicalize()
                .unwrap()
        }
        let mut sandbox = Bubblewrap::new(executable("python3"), executable("node"));
        sandbox.bwrap = executable("bwrap");
        sandbox
    }
    async fn tasks() -> (Kernel, Arc<dyn TaskAdmin>) {
        let kernel = Kernel::with_services(KernelServices::default());
        let plugin = eve_task_plugin::TaskPlugin::new().unwrap();
        let admin = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        (kernel, admin)
    }
    fn args(call_id: &str) -> Args {
        Args {
            action: "capture".into(),
            bundle_id: None,
            call_id: Some(call_id.into()),
            arguments: Some(json!({"value":7})),
            task_id: None,
            candidate: None,
        }
    }
    #[tokio::test]
    async fn real_failure_restart_scope_candidate_and_no_original_activation() {
        let root = tempfile::tempdir().unwrap();
        let (_kernel, tasks) = tasks().await;
        let scripts = ScriptStore::open(root.path().join("scripts")).unwrap();
        let sandbox = sandbox();
        let bad=scripts.publish(&scope(),spec("import sys,json\njson.loads(sys.stdin.readline())\nprint(json.dumps({'op':'return','value':7}),flush=True)\nsys.exit(17)\n")).unwrap();
        scripts.activate(&scope(), &bad.reference, None).unwrap();
        let runtime = ScriptRuntime::new(
            scripts.clone(),
            Arc::new(DenyTools),
            sandbox.clone(),
            root.path().join("artifacts"),
        );
        let receipt = runtime
            .run(
                &runtime.bind(&scope(), &bad.reference).unwrap(),
                "original",
                json!({"value":7}),
                context(),
            )
            .await
            .unwrap();
        assert_eq!(receipt.status, ReceiptStatus::Failed);
        assert_eq!(
            receipt.failure_diagnostic.as_ref().unwrap().exit_code,
            Some(17)
        );
        for status in [
            ReceiptStatus::Unknown,
            ReceiptStatus::Running,
            ReceiptStatus::Cancelled,
            ReceiptStatus::Succeeded,
        ] {
            let mut forbidden = receipt.clone();
            forbidden.status = status;
            assert!(validate_receipt(&forbidden).is_err());
        }
        let mut old = receipt.clone();
        old.failure_diagnostic = None;
        assert!(validate_receipt(&old).is_err());
        let mut external = receipt.clone();
        external.evidence.push(eve_script_runtime::Evidence {
            operation: "tool".into(),
            target: "send_message".into(),
            status: ReceiptStatus::Unknown,
            result_digest: None,
        });
        assert!(validate_receipt(&external).is_err());
        let state = Arc::new(FileStateStore::open(root.path().join("bundles")).unwrap());
        let service = Service {
            scripts: scripts.clone(),
            sandbox: sandbox.clone(),
            state,
            tasks: tasks.clone(),
            lock: Mutex::new(()),
        };
        let mut external_spec = spec("print('never run')");
        external_spec.name = "external".into();
        external_spec.tools = vec!["send_message".into()];
        let external = scripts.publish(&scope(), external_spec).unwrap();
        assert!(service.pure_version(&scope(), &external.reference).is_err());
        let mut mismatched = args("original");
        mismatched.arguments = Some(json!({"value":8}));
        assert!(
            service
                .capture(&scope(), &mismatched, crate::qq_tasks::now_ms())
                .is_err()
        );
        let now = crate::qq_tasks::now_ms();
        let owner = TaskOwner {
            session_id: scope().session_id,
            user_id: scope().user_id,
        };
        let task = tasks
            .create(
                &owner,
                eve_task_api::TaskSpec {
                    title: "修复原脚本".into(),
                    instructions: "隔离验证修复，保留原授权".into(),
                    max_steps: 10,
                    context: None,
                },
                "failure-original",
                now,
            )
            .unwrap();
        tasks.start(&task.id, now).unwrap();
        let frame = eve_llm_api::TurnCheckpoint {
            version: 1,
            scope: scope(),
            messages: vec![
                eve_llm_api::ChatMessage::text(
                    eve_llm_api::ChatRole::User,
                    "修复原脚本，不发送消息",
                ),
                eve_llm_api::ChatMessage::assistant_tool_calls(vec![ToolCall {
                    id: "original".into(),
                    name: "run_script_plugin".into(),
                    arguments: json!({"version":bad.reference,"arguments":{"value":7}}),
                }])
                .unwrap(),
                eve_llm_api::ChatMessage::tool_results(vec![
                    eve_llm_api::ToolResult::success(
                        "original",
                        serde_json::to_value(&receipt).unwrap(),
                    )
                    .unwrap(),
                ])
                .unwrap(),
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
        };
        tasks.checkpoint(&task.id, frame, now + 60000, now).unwrap();
        tasks
            .finish(
                &task.id,
                eve_task_api::TaskEnd::Blocked {
                    result: "等待隔离修复候选".into(),
                },
                eve_task_api::TaskUsage {
                    tool_calls: 1,
                    model_requests: 1,
                },
                now,
            )
            .unwrap();
        let mut capture_args = args("original");
        capture_args.task_id = Some(task.id.clone());
        assert!(
            service
                .capture(
                    &ContextScope {
                        session_id: "other".into(),
                        ..scope()
                    },
                    &capture_args,
                    now
                )
                .is_err()
        );
        let captured = service.capture(&scope(), &capture_args, now).unwrap();
        let id = captured["bundle_id"].as_str().unwrap().to_owned();
        let bundle = service.read(&scope(), &id, "inspect", now).unwrap();
        let result = service.replay(&bundle, None, context()).await.unwrap();
        assert_eq!(result["report"]["reproduced"], true);
        assert!(
            service
                .read(
                    &ContextScope {
                        session_id: "other".into(),
                        ..scope()
                    },
                    &id,
                    "inspect",
                    now
                )
                .is_err()
        );
        drop(service);
        let state = Arc::new(FileStateStore::open(root.path().join("bundles")).unwrap());
        let service = Service {
            scripts: scripts.clone(),
            sandbox,
            state,
            tasks: tasks.clone(),
            lock: Mutex::new(()),
        };
        let restored = service.read(&scope(), &id, "inspect", now).unwrap();
        assert!(restored.replay.unwrap().reproduced);
        let fixed=scripts.publish(&scope(),spec("import sys,json\nr=json.loads(sys.stdin.readline())\nprint(json.dumps({'op':'return','value':r['arguments']['value']}),flush=True)\n")).unwrap();
        let result = service
            .replay(&bundle, Some(&fixed.reference), context())
            .await
            .unwrap();
        assert_eq!(result["report"]["status"], "succeeded");
        assert_eq!(result["candidate_activated"], false);
        assert_eq!(
            scripts.selected(&scope(), "replay-demo").unwrap(),
            bad.reference
        );
        assert_eq!(scripts.receipt(&scope(), "original").unwrap(), receipt);
        let tool = ReplayTool(Arc::new(service));
        assert!(
            tool.execute(
                ToolCall {
                    id: "delegated-resume".into(),
                    name: "manage_failure_reproduction".into(),
                    arguments: json!({"action":"resume_original","bundle_id":id})
                },
                context().with_delegated(true)
            )
            .await
            .is_err()
        );
        assert_eq!(
            tasks.snapshot().unwrap().get(&task.id).unwrap().status,
            eve_task_api::TaskStatus::Blocked
        );
        let resumed = tool
            .execute(
                ToolCall {
                    id: "repair-resume".into(),
                    name: "manage_failure_reproduction".into(),
                    arguments: json!({"action":"resume_original","bundle_id":id}),
                },
                context(),
            )
            .await
            .unwrap();
        assert_eq!(resumed["task_id"], task.id);
        let restored = tasks.snapshot().unwrap().get(&task.id).unwrap().clone();
        assert_eq!(restored.spec.max_steps, 10);
        let execution = restored.execution.unwrap();
        assert_eq!(execution.deadline_at_ms, Some(now + 60000));
        assert_eq!(execution.recoveries, 1);
        assert_eq!(execution.checkpoint.unwrap().used_tool_calls, 1);
        let service = &tool.0;

        assert!(
            service
                .read(&scope(), &id, "inspect", now + RETENTION_MS)
                .is_err()
        );
    }
    #[test]
    fn fixture_refuses_credentials_text_depth_and_unknown_fields() {
        assert!(!safe_input(&json!({"api_key":12345}), 0));
        assert!(!safe_input(&json!({"value":"secret text"}), 0));
        assert!(!safe_input(&json!(0), 9));
        assert!(safe_input(&json!({"value":[3,true,null]}), 0));
        assert!(unknown_effect(&json!({"receipt":{"status":"Unknown"}}), 0));
        assert!(!unknown_effect(
            &json!({"status":"succeeded","note":"unknown root cause"}),
            0
        ));
        assert!(
            serde_json::from_value::<Args>(json!({"action":"capture","owner":"other"})).is_err()
        );
    }
    #[tokio::test]
    async fn corrupted_bundle_is_preserved_and_untrusted_identity_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let (_kernel, tasks) = tasks().await;
        let state = Arc::new(FileStateStore::open(root.path().join("ledger")).unwrap());
        state
            .set(
                &Service::namespace().unwrap(),
                KEY.into(),
                b"{broken".to_vec(),
            )
            .unwrap();
        let service = Arc::new(Service {
            scripts: ScriptStore::open(root.path().join("scripts")).unwrap(),
            sandbox: sandbox(),
            state: state.clone(),
            tasks,
            lock: Mutex::new(()),
        });
        assert!(service.load().is_err());
        assert_eq!(
            state
                .get(&Service::namespace().unwrap(), KEY)
                .unwrap()
                .unwrap(),
            b"{broken"
        );
        let tool = ReplayTool(service);
        assert!(
            tool.execute(
                ToolCall {
                    id: "x".into(),
                    name: "manage_failure_reproduction".into(),
                    arguments: json!({"action":"inspect","bundle_id":"forged"})
                },
                ToolExecutionContext::new()
            )
            .await
            .is_err()
        );
    }
}
