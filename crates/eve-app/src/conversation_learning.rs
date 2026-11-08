//! QQ/Web 共用的交流学习组合；只在后台做无工具分析，前台读取持久经验。
use crate::AppError;
use eve_llm_api::{
    ContextAssembler, ContextScope, ContextSnapshot, LlmError, LlmFuture, TurnInput,
};
use eve_training_api::{
    ConversationLearner, ConversationMessage, ConversationRole, TrainingService,
};
use ring::digest::{SHA256, digest};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

pub(crate) fn observe_with_metadata(
    training: &dyn TrainingService,
    scope: &ContextScope,
    id: &str,
    speaker: &str,
    role: ConversationRole,
    text: &str,
    metadata: Option<eve_training_api::ConversationMetadata>,
) -> eve_plugin_api::PluginResult<()> {
    let speaker: String = digest(&SHA256, speaker.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    training.observe_conversation(
        scope,
        ConversationMessage {
            id: id.into(),
            speaker,
            role,
            text: text.into(),
            metadata,
        },
    )
}

pub(crate) struct Context {
    pub inner: Arc<dyn ContextAssembler>,
    pub training: Arc<dyn TrainingService>,
}
pub(crate) struct Manage {
    pub training: Arc<dyn TrainingService>,
}
impl eve_llm_api::Tool for Manage {
    fn definition(&self) -> eve_llm_api::ToolDefinition {
        eve_llm_api::ToolDefinition {
            name: "manage_conversation_learning".into(),
            description: "查看、暂停、恢复或按用户明确要求重置当前交流范围的表达学习。真实对话及反馈在后台形成可修正经验；不更改模型权重或跨会话传播原文。reset同时重置当前表达统计和说法，原始聊天保留。".into(),
            argument_schema: serde_json::json!({"type":"object","properties":{"action":{"type":"string","enum":["inspect","pause","resume","reset"]}},"required":["action"],"additionalProperties":false}),
            required_permissions: vec![], concurrency: Some(eve_llm_api::ToolConcurrency::Serial {scope:"conversation-learning".into()}),
        }
    }
    fn validate_arguments(
        &self,
        args: &serde_json::Value,
    ) -> Result<(), eve_llm_api::ToolValidationError> {
        if args.as_object().is_some_and(|m| m.len() == 1)
            && args["action"]
                .as_str()
                .is_some_and(|a| ["inspect", "pause", "resume", "reset"].contains(&a))
        {
            Ok(())
        } else {
            Err(eve_llm_api::ToolValidationError {
                message: "交流学习操作无效".into(),
            })
        }
    }
    fn execute(
        &self,
        call: eve_llm_api::ToolCall,
        context: eve_llm_api::ToolExecutionContext,
    ) -> eve_llm_api::ToolFuture<'_> {
        Box::pin(async move {
            use eve_llm_api::ToolExecutionError as E;
            self.validate_arguments(&call.arguments)
                .map_err(|e| E::Rejected(e.message))?;
            let scope = context
                .scope()
                .filter(|_| !context.is_delegated())
                .ok_or_else(|| E::Rejected("交流学习设置需要当前真实对话身份".into()))?;
            context.check_review_evidence()?;
            let operation = call.arguments["action"].as_str().expect("validated");
            match operation {
                "pause" => self.training.set_enabled(scope, false),
                "resume" => self.training.set_enabled(scope, true),
                "reset" => self.training.reset_expression(scope),
                _ => Ok(()),
            }
            .map_err(|e| E::Failed(e.to_string()))?;
            let lessons = self
                .training
                .conversation_lessons(scope)
                .map_err(|e| E::Failed(e.to_string()))?;
            Ok(
                serde_json::json!({"enabled":self.training.enabled(scope).map_err(|e|E::Failed(e.to_string()))?,"scope":"current_conversation","lessons":lessons,"action":operation}),
            )
        })
    }
}
impl ContextAssembler for Context {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_scoped(input, None)
    }
    fn assemble_scoped(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_with_model_session(input, scope, None)
    }
    fn assemble_with_model_session(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
        model_session: Option<eve_llm_api::ModelSession>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let learned = eve_training_plugin::TrainingContext(self.training.clone())
                .assemble_with_model_session(input.clone(), scope.clone(), model_session.clone())
                .await?;
            let mut current = self
                .inner
                .assemble_with_model_session(input, scope, model_session.clone())
                .await?;
            current.revision.push_str(&format!(":{}", learned.revision));
            current.profile.push('\n');
            current.profile.push_str(&learned.profile);
            current.memories.extend(learned.memories);
            current.pinned.extend(learned.pinned);
            Ok(current)
        })
    }
}

pub(crate) struct Background {
    active: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: tokio::task::JoinHandle<Result<(), AppError>>,
}
impl Background {
    pub(crate) fn start(
        training: Arc<dyn TrainingService>,
        learner: Arc<dyn ConversationLearner>,
        gate: crate::qq_budget::Gate,
    ) -> Self {
        let (active, mut activation) = watch::channel(false);
        let (stop, mut stopped) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            let outcome = async {
                while !*activation.borrow() {
                    tokio::select! { biased; _ = stopped.changed() => return Ok(()), result = activation.changed() => if result.is_err() { return Ok(()); } }
                }
                loop {
                    if *stopped.borrow() { return Ok(()); }
                    if gate.admits_category(crate::qq_budget::CONVERSATION_LEARNING, 1)?
                        && let Some(batch) = training.take_conversation_batch(crate::qq_tasks::now_ms())? {
                        // 批次号只在当前范围递增；沿用学习请求的稳定匿名会话键，避免跨范围同号误判为重放。
                        let scope_hash = eve_llm_api::ModelSession::scoped("conversation-learning", &batch.scope)?
                            .session_id().to_owned();
                        let result = tokio::select! { biased;
                            _ = stopped.changed() => { return Ok(()); },
                            result = tokio::time::timeout(Duration::from_secs(60), learner.learn(batch.clone())) => result.unwrap_or(Err(LlmError::ProviderTimeout)),
                        };
                        match result {
                            Ok(drafts) => match training.finish_conversation_batch(&batch, drafts, crate::qq_tasks::now_ms()) {
                                Ok(saved) => eprintln!("EVE_CONVERSATION_LEARNING {}", serde_json::json!({"scope_hash":scope_hash,"batch":batch.id,"outcome":"learned","saved":saved})),
                                Err(error) => {
                                    // 持久提交失败或结果未知不是模型输出拒绝：不能再提交 fail，
                                    // 更不能拿旧快照覆盖可能已经保存的经验和消费位置。
                                    if !matches!(error, eve_plugin_api::PluginError::Task(_)) {
                                        eprintln!("EVE_CONVERSATION_LEARNING {}", serde_json::json!({"scope_hash":scope_hash,"batch":batch.id,"outcome":"storage_unconfirmed","reason":error.to_string()}));
                                        return Err(error.into());
                                    }
                                    eprintln!("EVE_CONVERSATION_LEARNING {}", serde_json::json!({"scope_hash":scope_hash,"batch":batch.id,"outcome":"rejected","reason":error.to_string()}));
                                    // 重置或停止已经令批次失效，不恢复它；其他输出错误按同批有限重试。
                                    training.fail_conversation_batch(&batch, crate::qq_tasks::now_ms())?;
                                }
                            },
                            Err(error) => {
                                eprintln!("EVE_CONVERSATION_LEARNING {}", serde_json::json!({"scope_hash":scope_hash,"batch":batch.id,"outcome":"failed","attempt":batch.attempt,"reason":error.to_string()}));
                                training.fail_conversation_batch(&batch, crate::qq_tasks::now_ms())?;
                            }
                        }
                    }
                    tokio::select! { biased; _ = stopped.changed() => return Ok(()), _ = tokio::time::sleep(Duration::from_secs(1)) => {} }
                }
            }.await;
            let _ = done.send(true);
            outcome
        });
        Self {
            active,
            stop,
            finished,
            task,
        }
    }
    pub(crate) fn activate(&self) {
        self.active.send_replace(true);
    }
    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub(crate) fn request_stop(&self) {
        self.stop.send_replace(true);
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "交流学习后台异常；原状态保留")?
    }
}
