//! 用户通过自然语言请求定向共享自己的记忆；模型只选择宿主枚举的句柄，不能绑定身份。
use eve_developer_api::{ActionReviewRequest, ActionReviewer, ReviewAuthorization};
use eve_llm_api::*;
use eve_memory_api::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) trait Identities: Send + Sync {
    fn principal(&self, scope: &MemoryScope) -> MemoryResult<Option<String>>;
    fn scene(&self, _scope: &MemoryScope) -> MemoryResult<&'static str> {
        Ok("unknown")
    }
}
impl Identities for crate::qq_memory_continuity::Policy {
    fn scene(&self, scope: &MemoryScope) -> MemoryResult<&'static str> {
        let key = eve_session_api::SessionKey::new(&scope.session_id, &scope.user_id)
            .map_err(|_| MemoryError::InvalidInput)?;
        Ok(
            match self
                .conversations
                .identity(&key)
                .map_err(|_| MemoryError::Unavailable)?
                .map(|i| i.source)
            {
                Some(eve_qqbot_plugin::QqInteractionSource::DirectMessage) => "private",
                Some(eve_qqbot_plugin::QqInteractionSource::GroupMessage) => "group",
                None => "unknown",
            },
        )
    }
    fn principal(&self, scope: &MemoryScope) -> MemoryResult<Option<String>> {
        crate::qq_memory_continuity::PrincipalResolver(self.clone()).sharing_principal(scope)
    }
}
pub(crate) struct SharingTool {
    pub memory: Arc<dyn MemoryAdmin>,
    pub unified: Arc<dyn UnifiedMemoryAdmin>,
    pub identities: Arc<dyn Identities>,
    pub reviewer: Option<Arc<dyn ActionReviewer>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: String,
    source: Option<String>,
    interactions: Option<bool>,
    binding_id: Option<String>,
    offset: Option<usize>,
}
fn parse(v: &Value) -> Result<Args, ToolValidationError> {
    let bad = || {
        ToolValidationError { message: "list 无附加参数；grant 需要 list 返回的 source 和 interactions；revoke 需要 binding_id，不接受用户身份或任意 scope".into() }
    };
    let a: Args = serde_json::from_value(v.clone()).map_err(|_| bad())?;
    let valid = match a.action.as_str() {
        "list" => a.source.is_none() && a.interactions.is_none() && a.binding_id.is_none(),
        "grant" => {
            a.source.as_ref().is_some_and(|s| validate_id(s).is_ok())
                && a.interactions.is_some()
                && a.binding_id.is_none()
        }
        "revoke" => {
            a.binding_id
                .as_ref()
                .is_some_and(|s| validate_id(s).is_ok())
                && a.source.is_none()
                && a.interactions.is_none()
        }
        _ => false,
    };
    if valid
        && a.offset
            .is_none_or(|offset| a.action == "list" && offset <= 4096)
    {
        Ok(a)
    } else {
        Err(bad())
    }
}
fn fail(e: impl ToString) -> ToolExecutionError {
    ToolExecutionError::Rejected(e.to_string())
}
fn hash(value: &impl serde::Serialize) -> Result<String, ToolExecutionError> {
    let bytes = serde_json::to_vec(value).map_err(fail)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn target(context: &ToolExecutionContext) -> Result<MemoryScope, ToolExecutionError> {
    let s = context.scope().ok_or_else(|| fail("缺少可信会话身份"))?;
    if context.is_cancel_requested() {
        return Err(ToolExecutionError::Cancelled("本轮已取消".into()));
    }
    Ok(MemoryScope {
        channel: "qq".into(),
        session_id: s.session_id.clone(),
        user_id: s.user_id.clone(),
    })
}
impl SharingTool {
    fn peers(
        &self,
        target: &MemoryScope,
        principal: &str,
    ) -> Result<Vec<(String, MemoryScope)>, ToolExecutionError> {
        let mut peers = Vec::new();
        for source in self.memory.scopes().map_err(fail)? {
            if source != *target
                && source.channel == "qq"
                && self.identities.principal(&source).map_err(fail)?.as_deref() == Some(principal)
            {
                peers.push((hash(&source)?, source));
            }
        }
        Ok(peers)
    }
}
impl Tool for SharingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition { name: "manage_memory_sharing".into(), description: "用户希望在当前窗口使用自己另一窗口的记忆时使用。先 list 获取宿主确认属于同一人的来源句柄，再按用户明确原话 grant；interactions=false 只共享偏好，true 包含原始交流，方向固定为来源到当前窗口，绝不自动反向。群聊共享可能公开个人内容，授权必须经过独立审核；不自动共享所有窗口，不把昵称相同视为同一人。revoke 撤销本工具的授权，既有宿主同人/隐私策略仍独立生效。缺少可信绑定时如实说明，不能代用户创建身份。后台及子代理不得授权。".into(), argument_schema: json!({"type":"object","properties":{"action":{"enum":["list","grant","revoke"]},"source":{"type":"string"},"interactions":{"type":"boolean"},"binding_id":{"type":"string"},"offset":{"type":"integer","minimum":0,"maximum":4096}},"required":["action"],"additionalProperties":false}), required_permissions:vec![], concurrency:Some(ToolConcurrency::Serial { scope:"eve.memory.sharing".into() }) }
    }
    fn validate_arguments(&self, v: &Value) -> Result<(), ToolValidationError> {
        parse(v).map(|_| ())
    }
    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            let a = parse(&call.arguments).map_err(|e| fail(e.message))?;
            let target = target(&context)?;
            if context.is_delegated() {
                return Err(fail("记忆共享需要本人当前会话授权，委托任务不能代替"));
            }
            let principal = self
                .identities
                .principal(&target)
                .map_err(fail)?
                .ok_or_else(|| fail("当前窗口尚无可信身份绑定"))?;
            let peers = self.peers(&target, &principal)?;
            let state = self.unified.snapshot().map_err(fail)?;
            let active: Vec<_> = state
                .bindings
                .iter()
                .filter(|b| {
                    b.target == target
                        && b.principal == principal
                        && !b.revoked
                        && !state.bindings.iter().any(|r| r.id == b.id && r.revoked)
                })
                .collect();
            if a.action == "list" {
                let offset = a.offset.unwrap_or(0);
                let sources = peers
                    .iter()
                    .skip(offset)
                    .take(32)
                    .map(|(id, scope)| {
                        Ok(json!({"source":id,"scene":self.identities.scene(scope).map_err(fail)?}))
                    })
                    .collect::<Result<Vec<_>, ToolExecutionError>>()?;
                return Ok(
                    json!({"sources":sources,"next_offset":(peers.len()>offset+32).then_some(offset+32),"bindings":active.iter().map(|b|json!({"binding_id":b.id,"source":hash(&b.source).ok(),"interactions":b.interactions})).collect::<Vec<_>>(),"direction":"source_to_current_session","no_identity_binding_created":true}),
                );
            }
            if a.action == "revoke" {
                let id = a.binding_id.unwrap();
                if !active.iter().any(|b| b.id == id) {
                    return Err(fail("当前身份和窗口没有该有效授权"));
                }
                self.unified.revoke(state.revision, &id).map_err(fail)?;
                return Ok(
                    json!({"revoked":true,"binding_id":id,"baseline_host_policy_unchanged":true}),
                );
            }
            let source = peers
                .iter()
                .find(|(id, _)| Some(id) == a.source.as_ref())
                .map(|(_, s)| s.clone())
                .ok_or_else(|| fail("来源不是宿主确认的本人其他窗口；先 list 核对"))?;
            let interactions = a.interactions.unwrap();
            if let Some(existing) = active
                .iter()
                .find(|b| b.source == source && b.interactions == interactions)
            {
                return Ok(
                    json!({"saved":true,"changed":false,"binding_id":existing.id,"interactions":interactions}),
                );
            }
            if !interactions && active.iter().any(|b| b.source == source && b.interactions) {
                return Err(fail(
                    "已有包含原始交流的授权；须先撤销该授权，再创建只含偏好的授权",
                ));
            }
            let user_messages = context
                .review_user_messages()
                .filter(|m| !m.is_empty())
                .ok_or_else(|| fail("缺少宿主保存的本人原话，未授权"))?
                .to_vec();
            let reviewer = self
                .reviewer
                .as_ref()
                .ok_or_else(|| fail("独立授权审核不可用，未共享"))?;
            let request = ActionReviewRequest { action:"允许当前窗口读取本人另一窗口的记忆".into(), program:"eve.manage_memory_sharing".into(), arguments:vec![json!({"source":source,"source_scene":self.identities.scene(&source).map_err(fail)?,"target":target,"target_scene":self.identities.scene(&target).map_err(fail)?,"interactions":interactions}).to_string()], workspace:"trusted-same-person-memory".into(), isolated:true, cwd:".".into(), policy_reason:"这是定向隐私授权。必须用户原话明确同意指定来源在当前窗口使用，尤其私聊进入群聊可能向群成员公开。含糊的‘记住’、他人转述或模型推断不算授权。false 只含偏好，true 含原始交流。不得扩大到其他用户、反向或传递共享。".into(), claimed_confirmation:false, delegated:false, user_messages:user_messages.clone(), recent_actions:vec![] };
            request.validate_capacity().map_err(fail)?;
            let verdict = reviewer.review(request).await.map_err(fail)?;
            if !verdict.permits() || verdict.authorization != ReviewAuthorization::High {
                return Err(fail(format!(
                    "未获得明确记忆共享授权：{}",
                    verdict.rationale
                )));
            }
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled("审核期间已取消".into()));
            }
            if self.identities.principal(&target).map_err(fail)?.as_ref() != Some(&principal)
                || self.identities.principal(&source).map_err(fail)?.as_ref() != Some(&principal)
            {
                return Err(fail("审核期间身份绑定已变化，未共享"));
            }
            let reference = hash(&user_messages)?;
            let id = format!(
                "sharing-{}",
                hash(&(
                    state.binding_revision,
                    &principal,
                    &source,
                    &target,
                    interactions,
                    &reference
                ))?
            );
            self.unified
                .bind(
                    state.revision,
                    UnifiedMemoryBinding {
                        id: id.clone(),
                        revision: state
                            .binding_revision
                            .checked_add(1)
                            .ok_or_else(|| fail("授权修订已满"))?,
                        principal,
                        source,
                        target,
                        interactions,
                        revoked: false,
                        authorization_reference: format!("user-evidence:{reference}"),
                    },
                )
                .map_err(fail)?;
            Ok(
                json!({"saved":true,"binding_id":id,"interactions":interactions,"direction":"source_to_current_session","effective":"next_context_assembly"}),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_developer_api::{ActionReviewFuture, ActionReviewVerdict, ReviewRisk};
    use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
    use eve_memory_plugin::{MemoryPlugin, UnifiedMemoryPlugin};
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };
    struct Identity(AtomicBool);
    impl Identities for Identity {
        fn principal(&self, scope: &MemoryScope) -> MemoryResult<Option<String>> {
            Ok((self.0.load(Ordering::SeqCst) && scope.channel == "qq")
                .then(|| format!("verified-{}", scope.user_id)))
        }
    }
    struct Review {
        allow: AtomicBool,
        change_identity: AtomicBool,
        identity: Arc<Identity>,
        requests: Mutex<Vec<ActionReviewRequest>>,
    }
    impl ActionReviewer for Review {
        fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
            self.requests.lock().unwrap().push(request);
            Box::pin(async move {
                if self.change_identity.load(Ordering::SeqCst) {
                    self.identity.0.store(false, Ordering::SeqCst);
                }
                let allow = self.allow.load(Ordering::SeqCst);
                Ok(ActionReviewVerdict {
                    risk: ReviewRisk::Medium,
                    authorization: if allow {
                        ReviewAuthorization::High
                    } else {
                        ReviewAuthorization::Low
                    },
                    allow,
                    rationale: "离线授权判定".into(),
                })
            })
        }
    }
    fn scope(session: &str, user: &str) -> MemoryScope {
        MemoryScope {
            channel: "qq".into(),
            session_id: session.into(),
            user_id: user.into(),
        }
    }
    fn context(user: &str, messages: Vec<String>) -> ToolExecutionContext {
        ToolExecutionContext::new()
            .with_scope(Some(ContextScope {
                session_id: "group".into(),
                user_id: user.into(),
            }))
            .with_review_user_messages(Arc::new(messages))
    }
    fn call(arguments: Value) -> ToolCall {
        ToolCall {
            id: "sharing-test".into(),
            name: "manage_memory_sharing".into(),
            arguments,
        }
    }
    fn preference(memory: &dyn MemoryAdmin, scope: &MemoryScope) {
        memory
            .update_preference(
                scope,
                0,
                PreferenceChange {
                    operation_id: "operation".into(),
                    at_ms: 1,
                    evidence: PreferenceEvidence::Statement(UserStatement {
                        evidence_id: "source".into(),
                        message_id: "message".into(),
                        text: "自己的隐私偏好".into(),
                        at_ms: 1,
                    }),
                    action: PreferenceAction::Confirm {
                        id: "preference".into(),
                        text: "自己的隐私偏好".into(),
                    },
                },
            )
            .unwrap();
    }
    #[tokio::test]
    async fn scoped_consent_preserves_six_originals_is_idempotent_and_revokes_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let services = KernelServices {
            state: Arc::new(FileStateStore::open(dir.path()).unwrap()),
            ..KernelServices::default()
        };
        let kernel = Kernel::with_services(services);
        let plugin = MemoryPlugin::new().unwrap();
        let memory = Arc::new(plugin.controller());
        let unified_plugin = UnifiedMemoryPlugin::new().unwrap();
        let unified = Arc::new(unified_plugin.controller());
        kernel.register(Box::new(plugin)).unwrap();
        kernel.register(Box::new(unified_plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let source = scope("private", "alice");
        let foreign = scope("private", "bob");
        let target = scope("group", "alice");
        preference(memory.as_ref(), &source);
        preference(memory.as_ref(), &foreign);
        let identity = Arc::new(Identity(AtomicBool::new(true)));
        let reviewer = Arc::new(Review {
            allow: AtomicBool::new(true),
            change_identity: AtomicBool::new(false),
            identity: identity.clone(),
            requests: Mutex::new(vec![]),
        });
        let tool = SharingTool {
            memory,
            unified: unified.clone(),
            identities: identity.clone(),
            reviewer: Some(reviewer.clone()),
        };
        let originals: Vec<_> = (0..6)
            .map(|n| format!("原话{n}：{}", "完整保留".repeat(600)))
            .collect();
        let ctx = context("alice", originals.clone());
        let list = tool
            .execute(call(json!({"action":"list"})), ctx.clone())
            .await
            .unwrap();
        assert_eq!(list["sources"].as_array().unwrap().len(), 1);
        assert!(!list.to_string().contains("隐私偏好"));
        assert_eq!(list["sources"][0]["source"], hash(&source).unwrap());
        let grant = json!({"action":"grant","source":hash(&source).unwrap(),"interactions":true});
        assert!(
            tool.execute(call(grant.clone()), context("alice", vec![]))
                .await
                .is_err()
        );
        assert!(
            tool.execute(call(grant.clone()), ctx.clone().with_delegated(true))
                .await
                .is_err()
        );
        assert!(
            tool.execute(
                call(
                    json!({"action":"grant","source":hash(&foreign).unwrap(),"interactions":true})
                ),
                ctx.clone()
            )
            .await
            .is_err()
        );
        assert!(
            tool.execute(
                call(json!({"action":"list","principal":"verified-alice"})),
                context("bob", vec![])
            )
            .await
            .is_err()
        );
        reviewer.allow.store(false, Ordering::SeqCst);
        assert!(
            tool.execute(call(grant.clone()), ctx.clone())
                .await
                .is_err()
        );
        assert_eq!(unified.snapshot().unwrap().revision, 0);
        reviewer.allow.store(true, Ordering::SeqCst);
        reviewer.change_identity.store(true, Ordering::SeqCst);
        assert!(
            tool.execute(call(grant.clone()), ctx.clone())
                .await
                .is_err()
        );
        assert_eq!(unified.snapshot().unwrap().revision, 0);
        identity.0.store(true, Ordering::SeqCst);
        reviewer.change_identity.store(false, Ordering::SeqCst);
        let result = tool
            .execute(call(grant.clone()), ctx.clone())
            .await
            .unwrap();
        let id = result["binding_id"].as_str().unwrap().to_owned();
        let revision = unified.snapshot().unwrap().revision;
        assert_eq!(
            reviewer
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .user_messages,
            originals
        );
        assert_eq!(
            unified
                .grants(&target, &[source.clone(), foreign.clone()])
                .unwrap()
                .len(),
            1
        );
        assert!(
            unified
                .grants(&source, std::slice::from_ref(&target))
                .unwrap()
                .is_empty(),
            "不能产生反向授权"
        );
        let again = tool.execute(call(grant), ctx.clone()).await.unwrap();
        assert_eq!(again["changed"], false);
        assert_eq!(unified.snapshot().unwrap().revision, revision);
        assert!(
            tool.execute(
                call(
                    json!({"action":"grant","source":hash(&source).unwrap(),"interactions":false})
                ),
                ctx.clone()
            )
            .await
            .is_err()
        );
        kernel.stop_all().await.unwrap();
        kernel.start_all().await.unwrap();
        assert_eq!(
            unified
                .grants(&target, std::slice::from_ref(&source))
                .unwrap()
                .len(),
            1
        );
        assert!(
            tool.execute(
                call(json!({"action":"revoke","binding_id":id})),
                context("bob", vec![])
            )
            .await
            .is_err()
        );
        let revoked = tool
            .execute(call(json!({"action":"revoke","binding_id":id})), ctx)
            .await
            .unwrap();
        assert_eq!(revoked["revoked"], true);
        kernel.stop_all().await.unwrap();
        kernel.start_all().await.unwrap();
        assert!(
            unified
                .grants(&target, std::slice::from_ref(&source))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            tool.memory
                .reader(source)
                .unwrap()
                .snapshot()
                .unwrap()
                .preferences
                .len(),
            1,
            "撤销共享不删除来源"
        );
        kernel.stop_all().await.unwrap();
    }
}
