//! QQ 装配的脚本插件工具适配；叶子工具与直接工具调用共用宿主授权策略。
mod permission;
use eve_llm_api::{
    Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext, ToolExecutionError,
    ToolFuture, ToolValidationError,
};
use eve_script_runtime::{
    AuthorizedToolDispatch, Bubblewrap, CheckedTools, MigrationScript, ScriptRuntime, ScriptSpec,
    ScriptStore, StateMigration, ToolAuthorizer, VersionRef,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};

type Result<T> = std::result::Result<T, ToolExecutionError>;
const NAMES: [&str; 4] = [
    "save_script_plugin",
    "list_script_plugins",
    "activate_script_plugin",
    "run_script_plugin",
];

/// 只有审过同步执行/身份传递语义的叶子才能进入脚本RPC。
/// 新插件叶子须由可信组合层显式加入；任务、子代理、工作流与管理入口不在此表。
pub fn is_synchronous_leaf(name: &str) -> bool {
    matches!(
        name,
        "echo"
            | "send_message"
            | "sleep"
            | "show_budget"
            | "list_files"
            | "read_file"
            | "search_files"
            | "write_file"
            | "edit_file"
            | "apply_patch"
            | "run_command"
            | "github_api"
            | "send_file"
            | "query_file_delivery"
            | "read_attachment"
            | "describe_image"
            | "read_raw_message"
            | "read_source"
            | "search_reference"
            | "read_reference"
            | "check_practice_draft"
            | "run_practice"
            | "search_memory"
            | "list_interests"
            | "list_goals"
            | "list_knowledge"
            | "list_practice"
            | "list_skills"
            | "list_forged_tools"
            | "list_plans"
            | "list_outreach"
            | "inspect_runtime_health"
    )
}
fn rejected(error: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Rejected(error.to_string())
}

struct LateDispatch(OnceLock<CheckedTools>);
impl AuthorizedToolDispatch for LateDispatch {
    fn preflight_tool(&self, name: &str, context: &ToolExecutionContext) -> Result<()> {
        self.0
            .get()
            .ok_or_else(|| rejected("脚本宿主叶子工具尚未装配"))?
            .preflight_tool(name, context)
    }
    fn preflight_call(&self, call: &ToolCall, context: &ToolExecutionContext) -> Result<()> {
        self.0
            .get()
            .ok_or_else(|| rejected("脚本宿主叶子工具尚未装配"))?
            .preflight_call(call, context)
    }
    fn dispatch(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.0
                .get()
                .ok_or_else(|| rejected("脚本宿主叶子工具尚未装配"))?
                .dispatch(call, context)
                .await
        })
    }
}

pub struct ScriptTools {
    store: Arc<ScriptStore>,
    runtime: ScriptRuntime,
    sandbox: Bubblewrap,
    dispatch: Arc<LateDispatch>,
    authorizer: Arc<dyn ToolAuthorizer>,
}
impl ScriptTools {
    pub fn open(
        directory: impl AsRef<Path>,
        sandbox: Bubblewrap,
        authorizer: Arc<dyn ToolAuthorizer>,
    ) -> Result<Arc<Self>> {
        let store = ScriptStore::open(directory.as_ref().join("state"))?;
        let dispatch = Arc::new(LateDispatch(OnceLock::new()));
        let runtime = ScriptRuntime::new(
            store.clone(),
            dispatch.clone(),
            sandbox.clone(),
            directory.as_ref().join("artifacts"),
        );
        Ok(Arc::new(Self {
            store,
            runtime,
            sandbox,
            dispatch,
            authorizer,
        }))
    }
    pub(crate) fn replay_source(&self) -> (Arc<ScriptStore>, Bubblewrap) {
        (self.store.clone(), self.sandbox.clone())
    }
    /// 在可信工具装配完成后仅调用一次。拒绝把脚本管理工具再次作为叶子，避免越权递归。
    pub fn bind_tools(&self, tools: Vec<Arc<dyn Tool>>) -> Result<()> {
        if tools
            .iter()
            .any(|tool| !is_synchronous_leaf(&tool.definition().name))
        {
            return Err(rejected(
                "脚本只能装配已审核同步叶子；不能装配任务、子代理、工作流或管理入口",
            ));
        }
        self.dispatch
            .0
            .set(CheckedTools::new(tools, self.authorizer.clone())?)
            .map_err(|_| rejected("脚本叶子已装配"))
    }
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = (0..4)
            .map(|kind| {
                Arc::new(ScriptTool {
                    service: self.clone(),
                    kind,
                }) as Arc<dyn Tool>
            })
            .collect();
        tools.push(permission::tool(self.dispatch.clone()));
        tools
    }
}
struct ScriptTool {
    service: Arc<ScriptTools>,
    kind: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Save {
    spec: ScriptSpec,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    call_id: Option<String>,
    version: Option<VersionRef>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Activate {
    version: VersionRef,
    migration: Option<StateMigration>,
    migration_script: Option<MigrationScript>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    version: VersionRef,
    arguments: Value,
    #[serde(default)]
    dry_run: bool,
}
impl ScriptTool {
    fn validate(&self, args: &Value) -> Result<()> {
        if serde_json::to_vec(args).map_err(rejected)?.len() > 1024 * 1024 {
            return Err(rejected("脚本工具参数超限"));
        }
        match self.kind {
            0 => {
                let args: Save = serde_json::from_value(args.clone()).map_err(rejected)?;
                args.spec.validate()?;
            }
            1 => {
                let args: List = serde_json::from_value(args.clone()).map_err(rejected)?;
                if args.call_id.is_some() && args.version.is_some() {
                    return Err(rejected("call_id 与 version 不能同时提供"));
                }
            }
            2 => {
                let _: Activate = serde_json::from_value(args.clone()).map_err(rejected)?;
            }
            _ => {
                let _: Run = serde_json::from_value(args.clone()).map_err(rejected)?;
            }
        }
        Ok(())
    }
}
impl Tool for ScriptTool {
    fn definition(&self) -> ToolDefinition {
        let (description, properties, required) = match self.kind {
            0 => (
                "保存不可变 Python/JavaScript/Rust 脚本插件版本，不运行业务代码；Rust先在可信Linux沙箱独立编译std-only候选，失败返回 saved=false、compiled=false 和 compilation 中的退出码/信号/最多8192字节实际诊断（truncated 表明截断），按诊断修复后重新保存，不得把失败当成功；编译扣共享预算。spec 含 name、language(python/javascript/rust)、source、dependencies(别名到{script,digest})、tools(允许叶子名)、state_schema({version,fields:{字段:string/number/integer/boolean/object/array/null}})、initial_state。进程通过stdin/stdout每行JSON协议调用宿主，不能获取凭据或直接网络。声明工具不授予权限。先使用dry_run预检；保存后需activate初始化状态。",
                json!({"spec":{"type":"object"}}),
                vec!["spec"],
            ),
            1 => (
                "无参数时列出当前可信会话的脚本版本摘要、启用版本及状态，不加载全部源码。修改或诊断脚本前传 version={script,digest} 读取该不可变版本完整源码和声明；call_id 查询真实调用回执，不能与 version 同时提供。只能读取当前可信会话自己的版本。unknown 表示结果未知，禁止自动重放。版本保存不代表业务成功或权限已授予。",
                json!({"call_id":{"type":"string"},"version":{"type":"object","properties":{"script":{"type":"string"},"digest":{"type":"string"}},"required":["script","digest"],"additionalProperties":false}}),
                vec![],
            ),
            2 => (
                "显式启用或回退不可变脚本版本{script,digest}。每版本保留独立状态；schema变化需migration={from:{script,digest},expected_revision,value}或migration_script={from:{script,digest},expected_revision,language,source}，两者互斥。纯迁移脚本从stdin读取state，只能输出一行{op:return,value:候选状态}，禁止工具/依赖/文件操作；隔离执行成功且源revision未变才原子启用。回退不重放外部操作。已有目标状态不能覆盖。",
                json!({"version":{"type":"object"},"migration":{"type":"object"},"migration_script":{"type":"object","properties":{"from":{"type":"object"},"expected_revision":{"type":"integer","minimum":0},"language":{"type":"string","enum":["python","javascript","rust"]},"source":{"type":"string"}},"required":["from","expected_revision","language","source"],"additionalProperties":false}}),
                vec!["version"],
            ),
            _ => (
                "按不可变version={script,digest}执行脚本。arguments为JSON；dry_run=true只预检声明图/身份/权限/运行时配置，不建临时文件、不扣预算、不写回执、不启动进程、不调用工具。运行中JSON行check_tool{name,arguments}可只读预检已声明叶子的实际参数，返回allowed/executed=false，不保留权限；实际tool重查，取消或预检运行失败停止。实际执行需Linux bubblewrap隔离，缺少时拒绝。子脚本与叶子共享宿主身份/权限/原子预算/取消/期限；失败停止、未知禁止重放。回执含版本、状态与证据。",
                json!({"version":{"type":"object"},"arguments":{},"dry_run":{"type":"boolean"}}),
                vec!["version", "arguments"],
            ),
        };
        ToolDefinition {
            name: NAMES[self.kind].into(),
            description: description.into(),
            argument_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.script-runtime".into(),
            }),
        }
    }
    fn validate_arguments(&self, args: &Value) -> std::result::Result<(), ToolValidationError> {
        self.validate(args).map_err(|error| ToolValidationError {
            message: error.to_string(),
        })
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate(&call.arguments)?;
            let owner = context
                .scope()
                .ok_or_else(|| rejected("脚本工具需要可信会话身份"))?;
            match self.kind {
                0 => {
                    let args: Save = serde_json::from_value(call.arguments).map_err(rejected)?;
                    let compiled = args.spec.language == eve_script_runtime::Language::Rust;
                    match self
                        .service
                        .runtime
                        .prepare_candidate(args.spec, context)
                        .await?
                    {
                        eve_script_runtime::CandidatePreparation::Ready(version) => Ok(
                            json!({"saved":true,"executed":false,"compiled":compiled,"version":version}),
                        ),
                        eve_script_runtime::CandidatePreparation::CompilationFailed(report) => Ok(
                            json!({"success":false,"saved":false,"executed":false,"compiled":false,"phase":"compilation","version":null,"compilation":report}),
                        ),
                    }
                }
                1 => {
                    let args: List = serde_json::from_value(call.arguments).map_err(rejected)?;
                    if let Some(id) = args.call_id {
                        return Ok(json!({"receipt":self.service.store.receipt(owner,&id)?}));
                    }
                    if let Some(version) = args.version {
                        return Ok(json!({"version":self.service.store.version(owner,&version)?}));
                    }
                    let versions = self.service.store.summaries(owner)?;
                    let versions = versions
                        .iter()
                        .map(|version| -> Result<Value> {
                            let selected = optional(
                                self.service
                                    .store
                                    .selected(owner, &version.reference.script),
                            )?;
                            let state =
                                optional(self.service.store.state(owner, &version.reference))?;
                            Ok(json!({"version":version,"selected":selected,"state":state}))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(json!({"versions":versions}))
                }
                2 => {
                    let args: Activate =
                        serde_json::from_value(call.arguments).map_err(rejected)?;
                    if let Some(migration) = args.migration_script {
                        if args.migration.is_some() {
                            return Err(rejected("migration与migration_script不能同时提供"));
                        }
                        let identity = serde_json::to_vec(&(owner, context.turn_id(), &call.id))
                            .map_err(rejected)?;
                        let call_id = format!(
                            "migration-{}",
                            ring::digest::digest(&ring::digest::SHA256, &identity)
                                .as_ref()
                                .iter()
                                .map(|byte| format!("{byte:02x}"))
                                .collect::<String>()
                        );
                        let receipt = self
                            .service
                            .runtime
                            .activate_with_script(&args.version, &call_id, migration, context)
                            .await?;
                        return Ok(
                            json!({"activated":args.version,"executed":true,"receipt":receipt}),
                        );
                    }
                    self.service
                        .store
                        .activate(owner, &args.version, args.migration)?;
                    Ok(json!({"activated":args.version,"executed":false}))
                }
                _ => {
                    let args: Run = serde_json::from_value(call.arguments).map_err(rejected)?;
                    let bound = self.service.runtime.bind(owner, &args.version)?;
                    if args.dry_run {
                        return Ok(
                            json!({"preflight":self.service.runtime.preflight(&bound,&context)?,"executed":false}),
                        );
                    }
                    // 把模型局部 ID 绑定可信作用域和轮次，避免不同会话常见的 call_1 相互冲突。
                    let identity = serde_json::to_vec(&(owner, context.turn_id(), &call.id))
                        .map_err(rejected)?;
                    let call_id = format!(
                        "script-{}",
                        ring::digest::digest(&ring::digest::SHA256, &identity)
                            .as_ref()
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    );
                    let receipt = self
                        .service
                        .runtime
                        .run(&bound, &call_id, args.arguments, context)
                        .await?;
                    Ok(
                        json!({"success":receipt.status==eve_script_runtime::ReceiptStatus::Succeeded,"receipt":receipt}),
                    )
                }
            }
        })
    }
}
fn optional<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(ToolExecutionError::Rejected(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ContextScope, SharedToolSteps};
    use eve_script_runtime::{Language, StateSchema};
    use std::collections::BTreeMap;

    struct Deny;
    impl ToolAuthorizer for Deny {
        fn authorize(&self, _: &str, _: Option<&Value>, _: &ToolExecutionContext) -> Result<()> {
            Err(rejected("测试授权拒绝"))
        }
    }
    fn context() -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(ContextScope {
                session_id: "test-session".into(),
                user_id: "test-user".into(),
            }))
            .with_turn_id(Some(1))
            .with_shared_steps(SharedToolSteps::new(4).unwrap())
    }
    fn spec() -> ScriptSpec {
        ScriptSpec { name:"adapter-example".into(),language:Language::Python,source:"import json\ninput()\nprint(json.dumps({'op':'return','value':{'local':True}}),flush=True)\n".into(), dependencies:BTreeMap::new(),tools:vec![],state_schema:StateSchema{version:1,fields:BTreeMap::new()},initial_state:json!({}) }
    }
    fn service(root: &Path) -> Arc<ScriptTools> {
        ScriptTools::open(root, sandbox(), Arc::new(Deny)).unwrap()
    }
    fn sandbox() -> Bubblewrap {
        let executable = std::env::current_exe().unwrap();
        let mut sandbox = Bubblewrap::new(executable.clone(), executable.clone());
        sandbox.bwrap = executable;
        sandbox
    }
    #[cfg(target_os = "linux")]
    fn isolated_sandbox() -> Bubblewrap {
        fn trusted(name: &str) -> std::path::PathBuf {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join(name))
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("Linux隔离验收缺少PATH运行时: {name}"))
                .canonicalize()
                .unwrap()
        }
        let mut sandbox = Bubblewrap::new(trusted("python3"), trusted("node"));
        sandbox.bwrap = trusted("bwrap");
        sandbox
    }
    async fn execute(
        tool: &Arc<dyn Tool>,
        id: &str,
        args: Value,
        context: ToolExecutionContext,
    ) -> Result<Value> {
        tool.execute(
            ToolCall {
                id: id.into(),
                name: tool.definition().name,
                arguments: args,
            },
            context,
        )
        .await
    }
    #[tokio::test]
    async fn adapter_reads_exact_cold_version_after_restart_without_cross_scope_access() {
        let root = tempfile::tempdir().unwrap();
        let host = service(root.path());
        let tools = host.tools();
        let ctx = context();
        let original = spec();
        let saved = execute(&tools[0], "save-old", json!({"spec":original}), ctx.clone())
            .await
            .unwrap();
        let reference = saved["version"]["reference"].clone();
        let mut updated = spec();
        updated.source.push_str("# new immutable version\n");
        execute(&tools[0], "save-new", json!({"spec":updated}), ctx.clone())
            .await
            .unwrap();
        drop(tools);
        drop(host);
        let host = service(root.path());
        let tools = host.tools();
        let before = std::fs::read(root.path().join("state/ledger.json")).unwrap();
        let listed = execute(&tools[1], "list", json!({}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(listed["versions"].as_array().unwrap().len(), 2);
        assert!(
            !serde_json::to_string(&listed)
                .unwrap()
                .contains("import json")
        );
        let read = execute(
            &tools[1],
            "read-old",
            json!({"version":reference}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(read["version"], saved["version"]);
        let other = ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: "another-session".into(),
            user_id: "test-user".into(),
        }));
        assert!(
            execute(&tools[1], "other", json!({"version":reference}), other)
                .await
                .is_err()
        );
        assert!(
            execute(
                &tools[1],
                "no-identity",
                json!({"version":reference}),
                ToolExecutionContext::new()
            )
            .await
            .is_err()
        );
        assert!(
            execute(
                &tools[1],
                "ambiguous",
                json!({"version":reference,"call_id":"call-1"}),
                ctx.clone()
            )
            .await
            .is_err()
        );
        let mut missing = reference.clone();
        missing["digest"] = json!("0".repeat(64));
        assert!(
            execute(
                &tools[1],
                "missing",
                json!({"version":missing}),
                ctx.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(
            before,
            std::fs::read(root.path().join("state/ledger.json")).unwrap()
        );
        assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 4);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn adapter_tools_save_activate_dry_run_run_and_inspect_real_receipt() {
        let root = tempfile::tempdir().unwrap();
        let service = ScriptTools::open(root.path(), isolated_sandbox(), Arc::new(Deny)).unwrap();
        service.bind_tools(vec![]).unwrap();
        assert!(service.bind_tools(vec![]).is_err());
        let tools = service.tools();
        let ctx = context();
        let saved = execute(&tools[0], "save", json!({"spec":spec()}), ctx.clone())
            .await
            .unwrap();
        assert_eq!(saved["executed"], false);
        let reference = saved["version"]["reference"].clone();
        execute(
            &tools[2],
            "activate",
            json!({"version":reference}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let before = std::fs::read(root.path().join("state/ledger.json")).unwrap();
        let budget = ctx.shared_steps().unwrap().remaining().unwrap();
        let preview = execute(
            &tools[3],
            "dry-run",
            json!({"version":reference,"arguments":{},"dry_run":true}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(preview["executed"], false);
        assert_eq!(budget, ctx.shared_steps().unwrap().remaining().unwrap());
        assert_eq!(
            before,
            std::fs::read(root.path().join("state/ledger.json")).unwrap()
        );
        let result = execute(
            &tools[3],
            "real-run",
            json!({"version":reference,"arguments":{}}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["receipt"]["result"], json!({"local":true}));
        let inspected = execute(
            &tools[1],
            "inspect",
            json!({"call_id":result["receipt"]["call_id"]}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(inspected["receipt"], result["receipt"]);
        assert!(
            execute(
                &tools[3],
                "real-run",
                json!({"version":reference,"arguments":{}}),
                ctx
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn adapter_does_not_turn_declared_tools_into_permissions() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.bind_tools(vec![]).unwrap();
        let tools = service.tools();
        let ctx = context();
        let mut source = spec();
        source.tools = vec!["qq.send_file".into()];
        let saved = execute(&tools[0], "save", json!({"spec":source}), ctx.clone())
            .await
            .unwrap();
        let reference = saved["version"]["reference"].clone();
        execute(
            &tools[2],
            "activate",
            json!({"version":reference}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert!(
            execute(
                &tools[3],
                "preview",
                json!({"version":reference,"arguments":{},"dry_run":true}),
                ctx.clone()
            )
            .await
            .is_err()
        );
        assert!(
            execute(
                &tools[3],
                "run",
                json!({"version":reference,"arguments":{}}),
                ctx
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn adapter_rejects_workflow_and_task_bypasses_even_when_authorizer_permits() {
        struct Permit;
        impl ToolAuthorizer for Permit {
            fn authorize(
                &self,
                _: &str,
                _: Option<&Value>,
                _: &ToolExecutionContext,
            ) -> Result<()> {
                Ok(())
            }
        }
        struct NonLeaf(&'static str);
        impl Tool for NonLeaf {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: self.0.into(),
                    description: "不可装配入口".into(),
                    argument_schema: json!({"type":"object"}),
                    required_permissions: vec![],
                    concurrency: None,
                }
            }
            fn validate_arguments(
                &self,
                _: &Value,
            ) -> std::result::Result<(), ToolValidationError> {
                Ok(())
            }
            fn execute(&self, _: ToolCall, _: ToolExecutionContext) -> ToolFuture<'_> {
                Box::pin(async { panic!("任务/工作流管理入口不得被脚本派发") })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let service = ScriptTools::open(root.path(), sandbox(), Arc::new(Permit)).unwrap();
        for name in [
            "run_workflow_tool",
            "save_workflow_tool",
            "dispatch_background_task",
            "run_subagent",
            "create_subagent",
            "save_script_plugin",
            "run_script_plugin",
            "inspect_tool_permission",
        ] {
            assert!(
                service.bind_tools(vec![Arc::new(NonLeaf(name))]).is_err(),
                "不能装配非叶子 {name}"
            );
        }
        // 拒绝装配不残留服务；即使脚本声明了旁路工具也无法通过预检。
        service.bind_tools(vec![]).unwrap();
        let tools = service.tools();
        let ctx = context();
        let mut source = spec();
        source.tools = vec!["run_workflow_tool".into()];
        let saved = execute(
            &tools[0],
            "save-bypass",
            json!({"spec":source}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let reference = saved["version"]["reference"].clone();
        execute(
            &tools[2],
            "activate-bypass",
            json!({"version":reference}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert!(
            execute(
                &tools[3],
                "run-bypass",
                json!({"version":reference,"arguments":{}}),
                ctx
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn adapter_portable_save_activate_and_list_do_not_require_native_execution() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.bind_tools(vec![]).unwrap();
        let tools = service.tools();
        let ctx = context();
        let saved = execute(
            &tools[0],
            "portable-save",
            json!({"spec":spec()}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let reference = saved["version"]["reference"].clone();
        execute(
            &tools[2],
            "portable-activate",
            json!({"version":reference}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let listed = execute(&tools[1], "portable-list", json!({}), ctx)
            .await
            .unwrap();
        assert_eq!(listed["versions"][0]["selected"], reference);
        assert_eq!(listed["versions"][0]["state"]["value"], json!({}));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn adapter_pure_migration_schema_and_execution_preserve_old_state_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let service = ScriptTools::open(root.path(), isolated_sandbox(), Arc::new(Deny)).unwrap();
        service.bind_tools(vec![]).unwrap();
        let tools = service.tools();
        let ctx = context();
        let owner = ctx.scope().unwrap();
        let from = service.store.publish(owner, spec()).unwrap().reference;
        service.store.activate(owner, &from, None).unwrap();
        let mut next = spec();
        next.state_schema.version = 2;
        let to = service.store.publish(owner, next).unwrap().reference;
        let migration = json!({"from":from,"expected_revision":0,"language":"python",
            "source":"import json\nx=json.loads(input())\nprint(json.dumps({'op':'return','value':x['state']['value']}))"});
        let result = execute(&tools[2], "mutually-exclusive", json!({"version":to,
            "migration":{"from":from,"expected_revision":0,"value":{}},"migration_script":migration}), ctx.clone()).await;
        assert!(matches!(result, Err(ToolExecutionError::Rejected(_))));
        let mut oversized = migration.clone();
        oversized["source"] = json!("x".repeat(eve_script_runtime::MAX_SOURCE_BYTES + 1));
        assert!(
            execute(
                &tools[2],
                "oversized",
                json!({"version":to,"migration_script":oversized}),
                ctx.clone()
            )
            .await
            .is_err()
        );
        let mut forbidden = migration.clone();
        forbidden["source"] =
            json!("print('{\"op\":\"tool\",\"name\":\"echo\",\"arguments\":{}}')");
        assert!(
            execute(
                &tools[2],
                "forbidden",
                json!({"version":to,"migration_script":forbidden}),
                ctx.clone()
            )
            .await
            .is_err()
        );
        assert_eq!(service.store.selected(owner, &from.script).unwrap(), from);
        assert!(service.store.state(owner, &to).is_err());
        let success = execute(
            &tools[2],
            "migrate-real",
            json!({"version":to,"migration_script":migration}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(success["executed"], true);
        assert_eq!(success["receipt"]["status"], "succeeded");
        assert_eq!(service.store.selected(owner, &to.script).unwrap(), to);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn adapter_rust_save_compiles_candidate_and_runs_without_host_rebuild() {
        let output = std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let compiler = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
            .join("bin/rustc");
        let root = tempfile::tempdir().unwrap();
        let service = ScriptTools::open(
            root.path(),
            isolated_sandbox().with_rustc(compiler),
            Arc::new(Deny),
        )
        .unwrap();
        service.bind_tools(vec![]).unwrap();
        let tools = service.tools();
        let ctx = context();
        let mut candidate = spec();
        candidate.language = Language::Rust;
        candidate.source = include_str!("../../script-runtime/examples/plugins/hello.rs").into();
        let saved = execute(
            &tools[0],
            "rust-save",
            json!({"spec":candidate}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(saved["compiled"], true);
        assert_eq!(saved["executed"], false);
        let version = saved["version"]["reference"].clone();
        execute(
            &tools[2],
            "rust-enable",
            json!({"version":version}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let result = execute(
            &tools[3],
            "rust-run",
            json!({"version":version,"arguments":{}}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(result["receipt"]["result"]["language"], "rust");
        candidate.source = "not valid Rust".into();
        let failed = execute(
            &tools[0],
            "rust-bad-save",
            json!({"spec":candidate}),
            ctx.clone(),
        )
        .await
        .unwrap();
        assert_eq!(failed["success"], false);
        assert_eq!(failed["saved"], false);
        assert_eq!(failed["compiled"], false);
        assert_eq!(failed["executed"], false);
        assert_eq!(failed["phase"], "compilation");
        assert_eq!(failed["version"], Value::Null);
        assert_eq!(failed["compilation"]["language"], "rust");
        assert_ne!(failed["compilation"]["exit_code"], 0);
        assert!(
            failed["compilation"]["diagnostics"]
                .as_str()
                .unwrap()
                .contains("error")
        );
        assert_eq!(failed["compilation"]["truncated"], false);
        assert_eq!(service.store.list(ctx.scope().unwrap()).unwrap().len(), 1);
        assert_eq!(
            serde_json::to_value(
                service
                    .store
                    .selected(ctx.scope().unwrap(), "adapter-example")
                    .unwrap()
            )
            .unwrap(),
            version
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn adapter_non_linux_dry_run_and_run_fail_closed_after_portable_save() {
        let root = tempfile::tempdir().unwrap();
        let service = service(root.path());
        service.bind_tools(vec![]).unwrap();
        let tools = service.tools();
        let ctx = context();
        let saved = execute(
            &tools[0],
            "unsupported-save",
            json!({"spec":spec()}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let reference = saved["version"]["reference"].clone();
        execute(
            &tools[2],
            "unsupported-activate",
            json!({"version":reference}),
            ctx.clone(),
        )
        .await
        .unwrap();
        let before = std::fs::read(root.path().join("state/ledger.json")).unwrap();
        for dry_run in [true, false] {
            let error = execute(
                &tools[3],
                "unsupported-run",
                json!({"version":reference,"arguments":{},"dry_run":dry_run}),
                ctx.clone(),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, ToolExecutionError::Rejected(_)));
            assert!(error.to_string().contains("Linux"));
        }
        assert_eq!(
            before,
            std::fs::read(root.path().join("state/ledger.json")).unwrap()
        );
        assert_eq!(ctx.shared_steps().unwrap().remaining().unwrap(), 4);
    }
}
