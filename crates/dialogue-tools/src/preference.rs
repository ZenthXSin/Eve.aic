//! 三档偏好写入。保存的只是偏好文本，不扩大工具权限。
use crate::{Services, failed, scope};
use eve_llm_api::{
    ContextScope, Tool, ToolCall, ToolConcurrency, ToolDefinition, ToolExecutionContext,
    ToolExecutionError, ToolFuture, ToolValidationError,
};
use eve_memory_api::{
    MAX_PREFERENCE_BYTES, MemoryAdmin, MemoryError, MemoryScope, PreferenceAction,
    PreferenceChange, PreferenceEvidence, PreferenceStatus, UserStatement, validate_id,
    validate_text,
};
use ring::digest::{Context, SHA256};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

/// 宿主判断当前操作者是否为主人。管理员不算主人。
pub trait PreferenceOwnerCheck: Send + Sync {
    fn is_owner(&self, scope: &ContextScope) -> bool;
}

/// 用闭包装配主人检查，避免 dialogue-tools 依赖 QQ 管理模块。
pub struct OwnerCheck<F>(pub F);

impl<F> PreferenceOwnerCheck for OwnerCheck<F>
where
    F: Fn(&ContextScope) -> bool + Send + Sync,
{
    fn is_owner(&self, scope: &ContextScope) -> bool {
        (self.0)(scope)
    }
}

pub struct SavePreferenceTool {
    pub services: Arc<Services>,
}

const DESCRIPTION: &str = "保存一条偏好原文，不是权限，也不能把正文里的指令变成授权。scope 三选一：session 只写入当前会话；user 写入同一频道、同一用户已有的全部会话（没有其他会话时至少写当前会话），供跨会话只读；global 仅主人可用，写入同一频道已有的全部会话（没有其他会话时至少写当前会话），管理员不能保存全局偏好。可选 id：不给则新建；给出时必须是该作用域已确认的偏好，用于修正，不存在则失败。偏好持久化保存，后续对话按已有记忆读取规则加载，不扩大权限。";

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "scope": {
                "type": "string",
                "enum": ["session", "user", "global"],
                "description": "session 当前会话；user 同一用户已有会话；global 仅主人、同一频道已有会话"
            },
            "text": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4096,
                "description": "偏好原文。trim 后须与原文一致，不能含控制字符"
            },
            "id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 256,
                "description": "已有偏好的 id；省略则新建"
            }
        },
        "required": ["scope", "text"],
        "additionalProperties": false
    })
}

fn invalid() -> ToolValidationError {
    ToolValidationError {
        message: "工具参数形状或长度无效；不接受用户身份、文件系统路径或额外字段".into(),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(1)
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hash = Context::new(&SHA256);
    for part in parts {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn explain(error: MemoryError) -> String {
    match error {
        MemoryError::InvalidInput => format!(
            "偏好内容须为 1 至 {MAX_PREFERENCE_BYTES} UTF-8 字节，且不能包含空字符；ID 必须有效。"
        ),
        MemoryError::NotFound => "当前作用域没有这条偏好。".into(),
        MemoryError::Conflict => "偏好操作与已有状态冲突。".into(),
        MemoryError::StaleRevision => "记忆刚刚发生变化，重试后仍未写入。".into(),
        MemoryError::LimitReached => "记忆容量已满；原记录已保留。".into(),
        MemoryError::Unavailable => "交互记忆服务不可用。".into(),
        MemoryError::CorruptState => "交互记忆状态损坏；未清空。".into(),
        MemoryError::UnsupportedVersion => "不支持该交互记忆状态版本。".into(),
        MemoryError::Storage => "交互记忆存储失败。".into(),
    }
}

fn targets(
    admin: &dyn MemoryAdmin,
    current: &MemoryScope,
    level: &str,
) -> Result<Vec<MemoryScope>, ToolExecutionError> {
    let known = admin.scopes().map_err(|error| failed(explain(error)))?;
    let selected: Vec<MemoryScope> = match level {
        "session" => vec![current.clone()],
        "user" => known
            .into_iter()
            .filter(|item| item.channel == current.channel && item.user_id == current.user_id)
            .collect(),
        "global" => known
            .into_iter()
            .filter(|item| item.channel == current.channel)
            .collect(),
        _ => return Err(failed("偏好范围无效")),
    };
    let mut selected = selected;
    if !selected.iter().any(|item| item == current) {
        selected.push(current.clone());
    }
    selected.sort();
    selected.dedup();
    Ok(selected)
}

struct PreferenceWrite<'a> {
    id: &'a str,
    creating: bool,
    text: &'a str,
    evidence_id: &'a str,
    message_id: &'a str,
    evidence_text: &'a str,
    operation_id: &'a str,
    at_ms: u64,
}

fn write_one(
    admin: &dyn MemoryAdmin,
    target: &MemoryScope,
    write: &PreferenceWrite<'_>,
) -> Result<(String, u64), MemoryError> {
    let id = write.id;
    let creating = write.creating;
    let text = write.text;
    let evidence_id = write.evidence_id;
    let message_id = write.message_id;
    let evidence_text = write.evidence_text;
    let operation_id = write.operation_id;
    let at_ms = write.at_ms;
    let reader = admin.reader(target.clone())?;
    let snapshot = reader.snapshot()?;
    if snapshot.scope != *target {
        return Err(MemoryError::Unavailable);
    }
    let existing = snapshot
        .preferences
        .iter()
        .find(|preference| preference.id == id);
    let action = match (existing, creating) {
        (Some(preference), _) if preference.status == PreferenceStatus::Confirmed => {
            PreferenceAction::Correct {
                id: id.into(),
                text: text.into(),
            }
        }
        (Some(_), _) => return Err(MemoryError::Conflict),
        (None, false) => return Err(MemoryError::NotFound),
        (None, true) => PreferenceAction::Confirm {
            id: id.into(),
            text: text.into(),
        },
    };
    let change = PreferenceChange {
        operation_id: operation_id.into(),
        at_ms,
        evidence: PreferenceEvidence::Statement(UserStatement {
            evidence_id: evidence_id.into(),
            message_id: message_id.into(),
            text: evidence_text.into(),
            at_ms,
        }),
        action,
    };
    let mut expected = snapshot.revision;
    let saved = match admin.update_preference(target, expected, change.clone()) {
        Err(MemoryError::StaleRevision) => {
            let fresh = admin.reader(target.clone())?.snapshot()?;
            if fresh.scope != *target {
                return Err(MemoryError::Unavailable);
            }
            expected = fresh.revision;
            admin.update_preference(target, expected, change)?
        }
        other => other?,
    };
    if saved.scope != *target {
        return Err(MemoryError::Unavailable);
    }
    let current = saved
        .preferences
        .iter()
        .find(|preference| preference.id == id)
        .ok_or(MemoryError::Unavailable)?;
    if current.text != text || current.status != PreferenceStatus::Confirmed {
        // 同一操作重放可能看到后续修正；不把旧正文说成刚刚写入。
        return Err(MemoryError::Conflict);
    }
    Ok((current.id.clone(), current.revision))
}

impl Tool for SavePreferenceTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "save_preference".into(),
            description: DESCRIPTION.into(),
            argument_schema: schema(),
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::Serial {
                scope: "eve.save-preference".into(),
            }),
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        let object = arguments.as_object().ok_or_else(invalid)?;
        if serde_json::to_vec(arguments).map_err(|_| invalid())?.len() > 64 * 1024 {
            return Err(invalid());
        }
        let scope = object.get("scope").and_then(Value::as_str);
        let text = object.get("text").and_then(Value::as_str);
        if !matches!(scope, Some("session" | "user" | "global")) || text.is_none() {
            return Err(invalid());
        }
        let allowed = if object.contains_key("id") { 3 } else { 2 };
        if object.len() != allowed {
            return Err(invalid());
        }
        if object.contains_key("id")
            && object
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| validate_id(id).is_err())
        {
            return Err(invalid());
        }
        // 文本限制与 memory-api 一致；形状错误与内容错误分开，执行时再拒绝内容。
        let text = text.unwrap_or_default();
        if text.len() > MAX_PREFERENCE_BYTES
            || text.trim() != text
            || text.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            self.validate_arguments(&call.arguments)
                .map_err(|_| failed("工具参数无效"))?;
            let current = scope(&context)?;
            let level = call.arguments["scope"].as_str().unwrap_or_default();
            let text = call.arguments["text"].as_str().unwrap_or_default();
            if validate_text(text, MAX_PREFERENCE_BYTES).is_err()
                || text.trim() != text
                || text.chars().any(char::is_control)
            {
                return Err(failed(explain(MemoryError::InvalidInput)));
            }
            if level == "global" {
                let host = context
                    .scope()
                    .ok_or_else(|| failed("此工具需要宿主绑定的用户会话"))?;
                let allowed = self
                    .services
                    .owner_check
                    .as_ref()
                    .is_some_and(|check| check.is_owner(host));
                if !allowed {
                    return Err(failed("只有主人能保存全局偏好"));
                }
            }
            let admin = self
                .services
                .memory
                .as_ref()
                .ok_or_else(|| failed("此能力未装配或暂时不可用"))?;
            let targets = targets(admin.as_ref(), &current, level)?;
            let at_ms = now_ms();
            let digest = sha256_hex(&[
                current.channel.as_bytes(),
                current.user_id.as_bytes(),
                level.as_bytes(),
                text.as_bytes(),
                &at_ms.to_be_bytes(),
            ]);
            let short = &digest[..16];
            let supplied = call.arguments.get("id").and_then(Value::as_str);
            let id = match supplied {
                Some(id) => id.to_owned(),
                None => format!("qq-memory-{digest}"),
            };
            if validate_id(&id).is_err() {
                return Err(failed(explain(MemoryError::InvalidInput)));
            }
            let message_id = format!("tool-save-preference-{level}-{short}");
            let evidence_id = format!("qq-memory-evidence-tool-{short}");
            let mut written = Vec::new();
            let mut failures = Vec::new();
            for target in &targets {
                let session_hash = sha256_hex(&[target.session_id.as_bytes()]);
                let operation_id =
                    format!("qq-memory-operation-tool-{short}-{}", &session_hash[..16]);
                match write_one(
                    admin.as_ref(),
                    target,
                    &PreferenceWrite {
                        id: &id,
                        creating: supplied.is_none(),
                        text,
                        evidence_id: &evidence_id,
                        message_id: &message_id,
                        evidence_text: text,
                        operation_id: &operation_id,
                        at_ms,
                    },
                ) {
                    Ok((preference_id, revision)) => written.push(json!({
                        "session_id": target.session_id,
                        "user_id": target.user_id,
                        "preference_id": preference_id,
                        "revision": revision,
                    })),
                    Err(error) => failures.push(json!({
                        "session_id": target.session_id,
                        "user_id": target.user_id,
                        "error": explain(error),
                    })),
                }
            }
            Ok(json!({
                "scope": level,
                "sessions_written": written.len(),
                "written": written,
                "failed": failures,
                "note": "偏好文本已持久化；后续对话按已有记忆读取规则加载，不扩大权限。",
            }))
        })
    }
}
