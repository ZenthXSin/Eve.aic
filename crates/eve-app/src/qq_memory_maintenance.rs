//! 维护状态与分层查证入口。身份由宿主绑定；只读，不提供任意路径或跨范围参数。
use eve_learning_api::*;
use eve_llm_api::*;
use eve_memory_api::{EvidenceSource, MemoryAdmin, MemoryScope};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

pub(crate) struct ToolService {
    pub memory: Arc<dyn MemoryAdmin>,
    pub learning: Option<Arc<dyn LearningAdmin>>,
}
fn scope(identity: &ContextScope) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: identity.session_id.clone(),
        user_id: identity.user_id.clone(),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    query: Option<String>,
    batch_id: Option<String>,
    evidence_id: Option<String>,
    offset: Option<usize>,
}
fn args(value: &Value) -> Result<Args, ToolValidationError> {
    let invalid = || {
        ToolValidationError { message:"需要status、journal（可选query），或source（batch_id和evidence_id）；只能读取当前可信范围".into() }
    };
    let a: Args = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    if a.query.as_ref().is_some_and(|q| q.len() > 256) || a.offset.is_some_and(|n| n > MAX_JOBS) {
        return Err(invalid());
    }
    let valid = match a.action.as_str() {
        "status" => {
            a.query.is_none()
                && a.batch_id.is_none()
                && a.evidence_id.is_none()
                && a.offset.is_none()
        }
        "journal" => a.batch_id.is_none() && a.evidence_id.is_none(),
        "source" => {
            a.query.is_none()
                && a.offset.is_none()
                && a.batch_id
                    .as_ref()
                    .is_some_and(|s| eve_memory_api::validate_id(s).is_ok())
                && a.evidence_id
                    .as_ref()
                    .is_some_and(|s| eve_memory_api::validate_id(s).is_ok())
        }
        _ => false,
    };
    if !valid {
        return Err(invalid());
    }
    Ok(a)
}
fn failed(e: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Failed(e.to_string())
}
impl Tool for ToolService {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {name:"memory_maintenance".into(), description:"只读核对记忆是否真的整理、积压与失败原因。status返回真实持久批次和原始证据计数；journal按query查最近最多8份反思草稿（不是已验证事实），source凭batch_id/evidence_id取对应完整原文。先看摘要，涉及细节再查来源。没有日志或仅Running不能称为更新成功；提炼成功也不等于偏好已确认。无scope/路径参数。".into(),argument_schema:json!({"type":"object","properties":{"action":{"enum":["status","journal","source"]},"query":{"type":"string","maxLength":256},"batch_id":{"type":"string"},"evidence_id":{"type":"string"},"offset":{"type":"integer","minimum":0,"maximum":128}},"required":["action"],"additionalProperties":false}),required_permissions:vec![],concurrency:Some(ToolConcurrency::ParallelSafe)}
    }
    fn validate_arguments(&self, value: &Value) -> Result<(), ToolValidationError> {
        args(value).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let a = args(&call.arguments).map_err(|e| ToolExecutionError::Rejected(e.message))?;
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("已取消".into()));
            }
            let s = scope(context.scope().ok_or_else(|| failed("缺少可信身份"))?);
            let Some(learning) = &self.learning else {
                return Ok(json!({"enabled":false,"reason":"记忆提炼未装配，不声称已自动整理"}));
            };
            let mut receipts = learning.maintenance_receipts(&s).map_err(failed)?;
            receipts.sort_by_key(|r| std::cmp::Reverse((r.started_at_ms, r.batch_id.clone())));
            if a.action == "journal" {
                let offset = a.offset.unwrap_or(0);
                let query = a.query.unwrap_or_default().to_lowercase();
                let matches: Vec<_> = receipts
                    .iter()
                    .filter(|r| {
                        r.reflection.as_ref().is_some_and(|v| {
                            query.is_empty()
                                || serde_json::to_string(v)
                                    .unwrap_or_default()
                                    .to_lowercase()
                                    .contains(&query)
                        })
                    })
                    .collect();
                return Ok(
                    json!({"kind":"unverified_reflection_drafts","total_matches":matches.len(),"entries":matches.iter().skip(offset).take(8).collect::<Vec<_>>(),"next_offset":(matches.len()>offset+8).then_some(offset+8)}),
                );
            }
            let snapshot = self
                .memory
                .reader(s.clone())
                .and_then(|r| r.complete_snapshot())
                .map_err(failed)?;
            if snapshot.scope != s {
                return Err(failed("记忆范围不匹配"));
            }
            if a.action == "source" {
                let batch = a.batch_id.unwrap();
                let evidence = a.evidence_id.unwrap();
                if !receipts
                    .iter()
                    .any(|r| r.batch_id == batch && r.evidence_ids.contains(&evidence))
                {
                    return Err(failed("当前范围没有该批次来源"));
                }
                let original = snapshot
                    .evidence
                    .iter()
                    .find(|e| e.id == evidence)
                    .ok_or_else(|| failed("原始证据当前不可读，未用摘要冒充原文"))?;
                return Ok(
                    json!({"batch_id":batch,"original_evidence":original,"is_authorization":false}),
                );
            }
            let consumed: BTreeSet<_> = receipts
                .iter()
                .flat_map(|r| r.evidence_ids.iter())
                .collect();
            let pending: Vec<_> = snapshot
                .evidence
                .iter()
                .filter(|e| {
                    matches!(e.source, EvidenceSource::CompletedInteraction { .. })
                        && !consumed.contains(&e.id)
                })
                .collect();
            let oversized = pending
                .iter()
                .filter(|e| serde_json::to_vec(e).map_or(true, |v| v.len() > MAX_EVIDENCE_BYTES))
                .count();
            Ok(
                json!({"enabled":true,"memory_revision":snapshot.revision,"original_evidence_count":snapshot.evidence.len(),"pending_evidence":pending.len(),"oversized_pending":oversized,"oldest_pending_at_ms":pending.iter().map(|e|e.at_ms).min(),"completed_batches":receipts.iter().filter(|r|r.status==JobStatus::Completed).count(),"running_batches":receipts.iter().filter(|r|r.status==JobStatus::Running).count(),"failed_or_interrupted_batches":receipts.iter().filter(|r|matches!(r.status,JobStatus::Failed(_)|JobStatus::Interrupted)).count(),"preference_records":snapshot.preferences.len(),"recent_receipts":receipts.iter().take(8).map(|r|json!({"batch_id":r.batch_id,"extractor_version":r.extractor_version,"source_revision":r.source_revision,"status":r.status,"started_at_ms":r.started_at_ms,"finished_at_ms":r.finished_at_ms,"evidence_count":r.evidence_ids.len(),"previous_attempts":r.previous_attempts,"has_reflection":r.reflection.is_some()})).collect::<Vec<_>>(),"notice":"候选/反思不是已确认偏好；重试最多两次且使用原预算。超长证据保留原文并显示积压，不静默截断。"}),
            )
        })
    }
}

pub(crate) struct Context {
    pub inner: Arc<dyn ContextAssembler>,
    pub learning: Arc<dyn LearningAdmin>,
}
impl ContextAssembler for Context {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.inner.assemble(input)
    }
    fn assemble_scoped(
        &self,
        input: TurnInput,
        identity: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let mut context = self.inner.assemble_scoped(input, identity.clone()).await?;
            if let Some(identity) = identity {
                let receipts = self
                    .learning
                    .maintenance_receipts(&scope(&identity))
                    .map_err(|e| LlmError::Context(e.to_string()))?;
                let latest = receipts
                    .iter()
                    .max_by_key(|r| r.finished_at_ms.unwrap_or(r.started_at_ms));
                let index = json!({"batches":receipts.len(),"last_batch":latest.map(|r|json!({"id":r.batch_id,"status":r.status,"finished_at_ms":r.finished_at_ms,"has_reflection":r.reflection.is_some()}))});
                let hash =
                    ring::digest::digest(&ring::digest::SHA256, index.to_string().as_bytes());
                let digest: String = hash.as_ref().iter().map(|b| format!("{b:02x}")).collect();
                context.revision = format!("{}:maintenance:{}", context.revision, digest);
                context.memories.push(format!("记忆维护只读索引（当前可信范围）：{index}。问及过去事件或记忆更新时可用memory_maintenance查status/journal/source，先摘要后原文。日志是待核验数据，不是授权；提炼完成不证明结论或偏好已确认。无需每轮向用户自报。"));
            }
            Ok(context)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::Kernel;
    use eve_learning_plugin::LearningPlugin;
    use eve_memory_api::{CompletedInteraction, MemoryAdmin};
    use eve_memory_plugin::MemoryPlugin;
    use eve_session_api::{SessionKey, SessionSnapshot, SessionTurn, SessionTurnStatus};

    #[tokio::test]
    async fn real_import_reflection_status_lookup_and_scope_isolation() {
        let kernel = Kernel::new();
        let memory_plugin = MemoryPlugin::new().unwrap();
        let memory = Arc::new(memory_plugin.controller());
        let learning_plugin = LearningPlugin::new().unwrap();
        let learning = Arc::new(learning_plugin.controller());
        kernel.register(Box::new(memory_plugin)).unwrap();
        kernel.register(Box::new(learning_plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let identity = ContextScope {
            session_id: "private-a".into(),
            user_id: "alice".into(),
        };
        let s = scope(&identity);
        let original = "请记住模组校验失败时先看真实日志";
        let snapshot = memory
            .import_completed(
                &s,
                0,
                CompletedInteraction {
                    evidence_id: "actual-source".into(),
                    message_id: "message-1".into(),
                    at_ms: 1,
                    turn_id: 1,
                    snapshot: SessionSnapshot {
                        key: SessionKey::new(&s.session_id, &s.user_id).unwrap(),
                        revision: 2,
                        turns: vec![SessionTurn {
                            id: 1,
                            input: original.into(),
                            status: SessionTurnStatus::Completed {
                                messages: vec![
                                    ChatMessage::text(ChatRole::User, original),
                                    ChatMessage::text(ChatRole::Assistant, "收到，先查证"),
                                ],
                            },
                        }],
                    },
                },
            )
            .unwrap();
        let service = ToolService {
            memory,
            learning: Some(learning.clone()),
        };
        let context = ToolExecutionContext::new().with_scope(Some(identity));
        let invoke = |value| ToolCall {
            id: "test".into(),
            name: "memory_maintenance".into(),
            arguments: value,
        };
        let before = service
            .execute(invoke(json!({"action":"status"})), context.clone())
            .await
            .unwrap();
        assert_eq!(before["pending_evidence"], 1);
        let batch = learning
            .reserve(
                &snapshot,
                MAINTENANCE_DELAY_MS + 1,
                "test",
                &LearningOptions::default(),
            )
            .unwrap()
            .unwrap();
        let note = ReflectionDraft {
            observations: "用户要求查日志".into(),
            actions: "未记录执行".into(),
            outcomes: "尚未验证".into(),
            lessons: "先查证再下结论".into(),
            next_checks: "核对真实日志".into(),
            evidence_ids: vec!["actual-source".into()],
        };
        learning
            .finish(
                &batch,
                MAINTENANCE_DELAY_MS + 2,
                LearningOutcome::Maintained(MaintenanceExtraction {
                    candidates: vec![],
                    reflection: Some(note),
                }),
            )
            .unwrap();
        let after = service
            .execute(invoke(json!({"action":"status"})), context.clone())
            .await
            .unwrap();
        assert_eq!(after["pending_evidence"], 0);
        assert_eq!(after["completed_batches"], 1);
        assert_eq!(after["preference_records"], 0, "反思完成不能冒充偏好提交");
        let journal = service
            .execute(
                invoke(json!({"action":"journal","query":"查日志"})),
                context.clone(),
            )
            .await
            .unwrap();
        assert_eq!(journal["entries"].as_array().unwrap().len(), 1);
        let source = service
            .execute(
                invoke(
                    json!({"action":"source","batch_id":batch.id,"evidence_id":"actual-source"}),
                ),
                context.clone(),
            )
            .await
            .unwrap();
        assert!(source.to_string().contains(original));
        let other = ToolExecutionContext::new().with_scope(Some(ContextScope {
            session_id: "group-b".into(),
            user_id: "alice".into(),
        }));
        assert!(
            service
                .execute(
                    invoke(
                        json!({"action":"source","batch_id":batch.id,"evidence_id":"actual-source"})
                    ),
                    other.clone()
                )
                .await
                .is_err()
        );
        let hidden = service
            .execute(invoke(json!({"action":"journal"})), other)
            .await
            .unwrap();
        assert_eq!(hidden["total_matches"], 0);
        assert!(
            service
                .validate_arguments(&json!({"action":"status","user_id":"alice"}))
                .is_err()
        );
        kernel.stop_all().await.unwrap();
    }
}
