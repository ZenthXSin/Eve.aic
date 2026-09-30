//! 同一路径连续独立启动：恢复完整会话，检查旧工具没有重放。无需真实 API。
#[path = "support/session_services.rs"]
mod services;
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::*;
use eve_plugin_api::{PluginId, PluginManifest, ServiceId};
use eve_runtime::{ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost};
use eve_session_api::*;
use eve_session_plugin::SessionPlugin;
use serde_json::json;
use std::{
    error::Error,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct RecoveryProvider {
    history: Vec<ChatMessage>,
    requests: AtomicUsize,
    cancel_stream: bool,
}
impl LlmProvider for RecoveryProvider {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        let round = self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            request.validate()?;
            let history_len = self.history.len();
            let current = if self.cancel_stream {
                "失败输入不得重放"
            } else if self.history.is_empty() {
                "生成回执"
            } else {
                "继续会话"
            };
            let tail_len = history_len + 1 + if round == 1 { 2 } else { 0 };
            let prefix_len = request
                .messages
                .len()
                .checked_sub(tail_len)
                .ok_or_else(|| LlmError::Protocol("恢复后请求消息缺失".into()))?;
            if request.messages[..prefix_len]
                .iter()
                .any(|m| m.role != ChatRole::System)
                || request.messages[prefix_len..prefix_len + history_len] != self.history
                || request.messages[prefix_len + history_len]
                    != ChatMessage::text(ChatRole::User, current)
                || request.tools.len() != 1
                || request.tools[0].name != "receipt"
            {
                return Err(LlmError::Protocol(
                    "恢复后的历史、系统前缀或工具不符".into(),
                ));
            }
            if self.cancel_stream {
                return Ok(ModelResponse::Final {
                    text: "这是一条被取消的部分回复".into(),
                });
            }
            if history_len == 0 && round == 0 {
                return Ok(ModelResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: "local-receipt-1".into(),
                        name: "receipt".into(),
                        arguments: json!({"text":"会话回执"}),
                    }],
                });
            }
            if history_len == 0 {
                let tail = &request.messages[request.messages.len() - 2..];
                if round != 1
                    || tail[0].tool_calls.len() != 1
                    || tail[0].tool_calls[0].id != "local-receipt-1"
                    || tail[1].tool_results
                        != vec![ToolResult::success(
                            "local-receipt-1",
                            json!({"receipt":"会话回执"}),
                        )?]
                {
                    return Err(LlmError::Protocol("本地调用或回执配对不符".into()));
                }
                Ok(ModelResponse::Final {
                    text: "已保存会话回执".into(),
                })
            } else if round == 0 {
                Ok(ModelResponse::Final {
                    text: "已恢复会话，未重放旧工具".into(),
                })
            } else {
                Err(LlmError::Protocol("恢复请求不应有额外工具轮次".into()))
            }
        })
    }
    fn stream<'a>(
        &'a self,
        request: ModelRequest,
        sink: &'a dyn ModelTextSink,
    ) -> LlmFuture<'a, ModelResponse> {
        Box::pin(async move {
            let response = self.complete(request).await?;
            if let ModelResponse::Final { text } = &response {
                for delta in text.chars().map(|c| c.to_string()) {
                    sink.text_delta(delta).await?;
                }
            }
            Ok(response)
        })
    }
}

#[derive(Default)]
struct Events {
    saved: AtomicUsize,
    deltas: AtomicUsize,
    cancel: bool,
}
impl TurnEventSink for Events {
    fn emit(&self, event: TurnEvent) -> LlmFuture<'_, ()> {
        Box::pin(async move {
            match event.kind {
                TurnEventKind::SessionSaved => {
                    self.saved.fetch_add(1, Ordering::SeqCst);
                }
                TurnEventKind::TextDelta { .. } => {
                    self.deltas.fetch_add(1, Ordering::SeqCst);
                    if self.cancel {
                        return Err(LlmError::Cancelled);
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = args.next().map(PathBuf::from).ok_or("请指定独立状态目录")?;
    let mode = args.next();
    let pending = mode.as_deref().is_some_and(|v| v == "--leave-pending");
    let cancel_stream = mode.as_deref().is_some_and(|v| v == "--cancel-stream");
    let streaming = cancel_stream || mode.as_deref().is_some_and(|v| v == "--stream");
    if mode.is_some() && !pending && !streaming {
        return Err("仅支持 <目录> [--leave-pending|--stream|--cancel-stream]".into());
    }
    if args.next().is_some() {
        return Err("参数过多".into());
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
    let result = async {
        kernel.start(&PluginId::new(services::OWNER)?).await?;
        kernel.start(&PluginId::new(SESSION_PLUGIN_ID)?).await?;
        let session = registry.get(&ServiceId::new(SESSION_SERVICE_ID)?)?.ok_or("会话服务不存在")?.value.downcast::<SessionServiceHandle>().map_err(|_| "会话服务类型不符")?.0.clone();
        let key = SessionKey::new("demo-session", "demo-user")?;
        let before = session.snapshot(&key)?;
        let history = before.as_ref().map_or_else(Vec::new, SessionSnapshot::history);
        let interrupted = before.as_ref().map_or(0, |s| s.turns.iter().filter(|t| t.status == SessionTurnStatus::Interrupted).count());
        if pending {
            let started = session.begin(SessionInput { key, text: "未完成输入，不应回放".into() })?;
            return Ok::<_, Box<dyn Error>>(json!({"mode":"pending","turn_id":started.lease.turn_id,"history_messages":history.len(),"interrupted":interrupted,"tool_executions":0,"provider_requests":0}));
        }
        let history_len = history.len();
        let provider = Arc::new(RecoveryProvider { history, requests: AtomicUsize::new(0), cancel_stream });
        let llm = LlmHost::new(provider.clone(), registry.clone(), kernel.clone(), permissions, ContextBinding { service_id: ServiceId::new(services::CONTEXT)?, expected_owner: PluginId::new(services::OWNER)? }, vec![ToolBinding { name: "receipt".into(), service_id: ServiceId::new(services::TOOL)?, expected_owner: PluginId::new(services::OWNER)? }], LlmHostConfig { response_mode: if streaming { ResponseMode::Stream } else { ResponseMode::Complete }, ..LlmHostConfig::default() })?;
        let host = SessionLlmHost::new(llm, SessionBinding::builtin()).with_logger(logger);
        let events = Events { cancel: cancel_stream, ..Events::default() };
        let input = SessionInput { key: key.clone(), text: if cancel_stream { "失败输入不得重放" } else if history_len == 0 { "生成回执" } else { "继续会话" }.into() };
        let result = host.run_turn_with_events(input, &events).await;
        if cancel_stream {
            if !matches!(result, Err(eve_runtime::SessionRunError::Turn(eve_runtime::TurnFailure { error: LlmError::Cancelled, .. })))
                || events.saved.load(Ordering::SeqCst) != 0 || events.deltas.load(Ordering::SeqCst) != 1
                || executions.load(Ordering::SeqCst) != 0 { return Err("流取消验收不符".into()); }
            let snapshot = session.snapshot(&key)?.ok_or("取消后会话缺失")?;
            if snapshot.history().len() != history_len || !matches!(snapshot.turns.last().map(|t| &t.status), Some(SessionTurnStatus::Failed { .. })) { return Err("取消输入不能进入完成历史".into()); }
            return Ok(json!({"mode":"cancelled","turn_id":snapshot.turns.last().unwrap().id,"history_messages":history_len,"tool_executions":0,"provider_requests":provider.requests.load(Ordering::SeqCst),"saved_events":0}));
        }
        let output = result?;
        if events.saved.load(Ordering::SeqCst) != 1 || streaming && events.deltas.load(Ordering::SeqCst) == 0 { return Err("流式输出或保存通知缺失".into()); }
        let starts = executions.load(Ordering::SeqCst);
        if starts != usize::from(history_len == 0) { return Err("旧工具被重新执行或首次工具未执行".into()); }
        let snapshot = session.snapshot(&key)?.ok_or("保存后会话缺失")?;
        snapshot.validate()?;
        if snapshot.history().len() != history_len + output.output.transcript.len() { return Err("完整历史记录长度不符".into()); }
        Ok(json!({"mode":"completed","streaming":streaming,"saved_events":events.saved.load(Ordering::SeqCst),"turn_id":output.turn_id,"history_messages":history_len,"interrupted":interrupted,"tool_executions":starts,"provider_requests":provider.requests.load(Ordering::SeqCst),"reply":output.output.text}))
    }.await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    let report = result?;
    stopped?;
    flushed?;
    println!("{report}");
    Ok(())
}
