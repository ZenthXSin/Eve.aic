//! 部分回复 → Mock LLM 自然语言判断 → 混合修订 → 工具闭环与独立进程恢复。
#[path = "support/session_services.rs"]
mod services;
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_control_api::*;
use eve_control_plugin::ControlPlugin;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::*;
use eve_message_api::*;
use eve_message_plugin::{MessageRouterPlugin, RelationPlugin};
use eve_plugin_api::{PluginDependency, PluginId, PluginManifest, ServiceId};
use eve_runtime::{
    ContextBinding, LlmHost, LlmHostConfig, LlmRelationJudge, SessionBinding, SessionControlRunner,
    SessionLlmHost,
};
use eve_session_api::*;
use eve_session_plugin::SessionPlugin;
use serde_json::json;
use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

struct Provider {
    requests: AtomicUsize,
    judgments: AtomicUsize,
}
impl LlmProvider for Provider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        Box::pin(async move {
            request.validate()?;
            self.judgments.fetch_add(1, Ordering::SeqCst);
            if !request.tools.is_empty() || request.messages.len() != 2 {
                return Err(LlmError::Protocol("判断请求不应包含工具或完整历史".into()));
            }
            let state: serde_json::Value = serde_json::from_str(
                request.messages[1]
                    .text
                    .as_deref()
                    .ok_or_else(|| LlmError::Protocol("判断输入缺失".into()))?,
            )
            .map_err(|_| LlmError::Protocol("判断输入不是 JSON".into()))?;
            if state["task_text"] != "旧任务" || state.get("target").is_some() {
                return Err(LlmError::Protocol("判断状态或数据边界不符".into()));
            }
            let parts = match state["message"].as_str() {
                Some("请调整一下") => json!([
                    {"intent":"ambiguous","confidence":0,"text":null}
                ]),
                Some("包含本地回执\n回执内容改为新任务回执") => json!([
                    {"intent":"supplement","confidence":95,"text":"包含本地回执"},
                    {"intent":"correction","confidence":95,"text":"回执内容改为新任务回执"}
                ]),
                _ => return Err(LlmError::Protocol("没有额外判断请求".into())),
            };
            Ok(ModelResponse::Final {
                text: json!({"parts":parts,"explanation":"按样本标注返回，不代表真实模型质量"})
                    .to_string(),
            })
        })
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            request.validate()?;
            self.requests.fetch_add(1, Ordering::SeqCst);
            let last = request
                .messages
                .last()
                .ok_or_else(|| LlmError::Protocol("消息缺失".into()))?;
            if last.text.as_deref() == Some("旧任务") {
                sink.text_delta("这是一条被取消的部分回复".into()).await?;
                return std::future::pending().await;
            }
            if last.role == ChatRole::User {
                let revision: serde_json::Value = serde_json::from_str(
                    last.text
                        .as_deref()
                        .ok_or_else(|| LlmError::Protocol("修订文字缺失".into()))?,
                )
                .map_err(|_| LlmError::Protocol("修订格式无效".into()))?;
                if revision["base_request"] != "旧任务"
                    || revision["changes"]
                        != json!([
                    {"kind":"supplement","text":"包含本地回执"}, {"kind":"correction","text":"回执内容改为新任务回执"}])
                {
                    return Err(LlmError::Protocol("原文要求未正确组合".into()));
                }
                return Ok(ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "controlled-receipt".into(),
                        name: "receipt".into(),
                        arguments: json!({"text":"新任务回执"}),
                    }],
                });
            }
            if last.tool_results
                != vec![ToolResult::success(
                    "controlled-receipt",
                    json!({"receipt":"新任务回执"}),
                )?]
            {
                return Err(LlmError::Protocol("工具回执不符".into()));
            }
            sink.text_delta("新任务已完成".into()).await?;
            Ok(ModelResponse::Final {
                text: "新任务已完成".into(),
            })
        })
    }
}
struct Channel(mpsc::Sender<ControlEvent>);
impl ControlEventSink for Channel {
    fn emit(&self, event: ControlEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move { self.0.send(event).await.map_err(|_| LlmError::Cancelled) })
    }
    fn closed(&self) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            self.0.closed().await;
            Ok(())
        })
    }
}
fn request(key: &SessionKey, task: &str, text: &str) -> ControlInput {
    ControlInput {
        session: SessionInput {
            key: key.clone(),
            text: text.into(),
        },
        task_id: task.into(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let temporary = tempfile::tempdir()?;
    let directory = args
        .next()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    if args.next().is_some() {
        return Err("仅支持 [独立状态目录]".into());
    }
    let backends = KernelServices {
        state: Arc::new(FileStateStore::open(&directory)?),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = Kernel::with_services(backends);
    let executions = Arc::new(AtomicUsize::new(0));
    kernel.register(Box::new(services::ServicesPlugin {
        manifest: PluginManifest::new(services::OWNER, "0.1.0")?,
        executions: executions.clone(),
    }))?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    kernel.register(Box::new(ConfigPlugin::new(ConfigBootstrap::new(
        directory.join("configuration"),
        vec![message_schema()],
    ))?))?;
    kernel.register(Box::new(MessageRouterPlugin::builtin()?))?;
    let provider = Arc::new(Provider {
        requests: AtomicUsize::new(0),
        judgments: AtomicUsize::new(0),
    });
    kernel.register(Box::new(RelationPlugin::with_fallback(
        None,
        Arc::new(LlmRelationJudge::new(provider.clone())),
    )?))?;
    let llm = LlmHost::new(
        provider.clone(),
        registry.clone(),
        kernel.clone(),
        permissions,
        ContextBinding {
            service_id: ServiceId::new(services::CONTEXT)?,
            expected_owner: PluginId::new(services::OWNER)?,
        },
        vec![ToolBinding {
            name: "receipt".into(),
            service_id: ServiceId::new(services::TOOL)?,
            expected_owner: PluginId::new(services::OWNER)?,
        }],
        LlmHostConfig {
            response_mode: ResponseMode::Stream,
            ..LlmHostConfig::default()
        },
    )?;
    let runner = Arc::new(SessionControlRunner::new(Arc::new(
        SessionLlmHost::new(llm, SessionBinding::builtin()).with_logger(logger),
    )));
    kernel.register(Box::new(ControlPlugin::new(
        runner,
        [services::OWNER, SESSION_PLUGIN_ID]
            .into_iter()
            .map(|owner| {
                Ok(PluginDependency {
                    id: PluginId::new(owner)?,
                    requirement: Some("^0.1".into()),
                })
            })
            .collect::<eve_plugin_api::PluginResult<Vec<_>>>()?,
    )?))?;
    let result = async {
        kernel.start(&PluginId::new(ROUTER_PLUGIN_ID)?).await?;
        let control = registry
            .get(&ServiceId::new(CONTROL_SERVICE_ID)?)?
            .ok_or("控制服务缺失")?
            .value
            .downcast::<ControlServiceHandle>()
            .map_err(|_| "控制服务类型不符")?
            .0
            .clone();
        let messages = registry
            .get(&ServiceId::new(ROUTER_SERVICE_ID)?)?
            .ok_or("消息服务缺失")?
            .value
            .downcast::<MessageServiceHandle>()
            .map_err(|_| "消息服务类型不符")?
            .0
            .clone();
        let session = registry
            .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
            .ok_or("会话服务缺失")?
            .value
            .downcast::<SessionServiceHandle>()
            .map_err(|_| "会话服务类型不符")?
            .0
            .clone();
        let key = SessionKey::new("message-session", "demo-user")?;
        let before = session.snapshot(&key)?.map_or(0, |s| s.history().len());
        let (tx, mut rx) = mpsc::channel(1);
        let old = control.submit(request(&key, "old-task", "旧任务"), Arc::new(Channel(tx)))?;
        let started = rx.recv().await.ok_or("事件流提前结束")?;
        let partial = rx.recv().await.ok_or("没有部分回复")?;
        if !control.accepts(&partial)
            || !matches!(partial.event.kind, TurnEventKind::TextDelta { .. })
        {
            return Err("部分回复或代际标识无效".into());
        }
        let unknown = messages.submit(
            IncomingMessage {
                target: old.clone(),
                message_id: "ambiguous".into(),
                text: "请调整一下".into(),
                reply_to: None,
            },
            Arc::new(DiscardControlEvents),
        )?;
        if !matches!(
            messages.wait(&unknown).await?.outcome,
            RouteOutcome::Clarify {
                reason: ClarifyReason::LowConfidence,
                ..
            }
        ) {
            return Err("模糊输入没有澄清".into());
        }
        let (tx, mut rx) = mpsc::channel(1);
        let ticket = messages.submit(
            IncomingMessage {
                target: old.clone(),
                message_id: "revision".into(),
                text: "包含本地回执\n回执内容改为新任务回执".into(),
                reply_to: None,
            },
            Arc::new(Channel(tx)),
        )?;
        let routed = messages.wait(&ticket).await?;
        let RouteOutcome::Replaced {
            generation: current,
            prior: cancelled,
        } = routed.outcome
        else {
            return Err("消息未触发重规划".into());
        };
        if current.task_id != old.task_id
            || cancelled.run.commit != CommitState::Failed
            || cancelled.run.started_tools != Some(0)
            || cancelled.run.failure != Some(RunFailure::Execution(LlmError::Cancelled))
            || control.accepts(&partial)
        {
            return Err("取消未收尾或旧回复仍可展示".into());
        }
        let mut saved = 0;
        loop {
            let event = rx.recv().await.ok_or("新事件流提前结束")?;
            if !control.accepts(&event) || event.key != current {
                return Err("旧代事件混入当前回复".into());
            }
            if event.event.kind == TurnEventKind::SessionSaved {
                saved += 1;
                break;
            }
        }
        let report = control.wait(&current).await?;
        let snapshot = session.snapshot(&key)?.ok_or("会话缺失")?;
        snapshot.validate()?;
        if report.run.commit != CommitState::Completed
            || report.run.started_tools != Some(1)
            || report.run.tool_results.len() != 1
            || executions.load(Ordering::SeqCst) != 1
            || snapshot.history().len() != before + 4
            || control.accepts(&started)
            || !matches!(snapshot.turns[snapshot.turns.len()-2].status, SessionTurnStatus::Failed { ref failure } if failure.code == SessionFailureCode::Cancelled && failure.started_tools == Some(0))
        {
            return Err("历史、工具次数或旧事件过滤验收失败".into());
        }
        Ok::<_, Box<dyn Error>>(
            json!({"cancelled_generation":old.generation,"completed_generation":current.generation,
            "turn_id":report.run.turn_id,"history_messages":snapshot.history().len(),"tool_executions":executions.load(Ordering::SeqCst),
            "provider_requests":provider.requests.load(Ordering::SeqCst),"judge_requests":provider.judgments.load(Ordering::SeqCst),"saved_events":saved,"old_event_accepted":control.accepts(&partial),"intent_parts":routed.decision.as_ref().map_or(0,|d|d.parts.len())}),
        )
    };
    let result = tokio::time::timeout(Duration::from_secs(10), result).await;
    if !matches!(&result, Ok(Ok(_)))
        && let Some(entry) = registry.get(&ServiceId::new(CONTROL_SERVICE_ID)?)?
        && let Ok(handle) = entry.value.downcast::<ControlServiceHandle>()
        && let Some(snapshot) = handle
            .0
            .snapshot(&SessionKey::new("message-session", "demo-user")?)?
    {
        handle.0.cancel(&snapshot.key)?;
        tokio::time::timeout(Duration::from_secs(3), handle.0.wait(&snapshot.key)).await??;
    }
    let stopped = kernel.stop_all().await;
    let output = result??;
    stopped?;
    println!("{output}");
    Ok(())
}
