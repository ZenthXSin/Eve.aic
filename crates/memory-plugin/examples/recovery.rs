//! 对同一独立目录连续运行：首轮保存交互证据、确认、更正与撤销；重启只读恢复。
//! 使用真实 Kernel、SessionPlugin 和 FileStateStore，不调用模型、工具或进程信号。
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryPlugin};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use eve_session_api::{SESSION_SERVICE_ID, SessionInput, SessionKey, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde_json::json;
use std::{error::Error, path::PathBuf, sync::Arc};

fn confirm() -> PreferenceChange {
    PreferenceChange {
        operation_id: "confirm-style".into(),
        at_ms: 2,
        evidence: PreferenceEvidence::Existing("interaction-1".into()),
        action: PreferenceAction::Confirm {
            id: "style".into(),
            text: "先给结论".into(),
        },
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let directory = args
        .next()
        .map(PathBuf::from)
        .ok_or("用法：recovery <独立状态目录>")?;
    if args.next().is_some() {
        return Err("参数过多：只接受独立状态目录".into());
    }
    let store = Arc::new(FileStateStore::open(&directory)?);
    let owner = PluginId::new(MEMORY_PLUGIN_ID)?;
    let before = store.get(&owner, MEMORY_STATE_KEY)?;
    let backends = KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let plugin = MemoryPlugin::new()?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    let result = async {
        kernel.start_all().await?;
        let sessions = registry
            .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
            .ok_or("会话服务不存在")?
            .value
            .downcast::<SessionServiceHandle>()
            .map_err(|_| "会话服务类型不符")?
            .0
            .clone();
        let scope = MemoryScope {
            channel: "example".into(),
            session_id: "recovery-session".into(),
            user_id: "recovery-user".into(),
        };
        let key = SessionKey::new(&scope.session_id, &scope.user_id)?;
        let reader = admin.reader(scope.clone())?;
        let restored_revision = reader.snapshot()?.revision;
        if before.is_none() {
            let input = "请先给结论，再给必要细节。";
            let turn = sessions.begin(SessionInput {
                key: key.clone(),
                text: input.into(),
            })?;
            sessions.complete(
                &turn.lease,
                vec![
                    ChatMessage::text(ChatRole::User, input),
                    ChatMessage::text(ChatRole::Assistant, "好的，先给结论。"),
                ],
            )?;
            let completed = sessions.snapshot(&key)?.ok_or("已完成的会话缺失")?;
            admin.import_completed(
                &scope,
                0,
                CompletedInteraction {
                    evidence_id: "interaction-1".into(),
                    message_id: "message-1".into(),
                    at_ms: 1,
                    snapshot: completed,
                    turn_id: turn.lease.turn_id,
                },
            )?;
            admin.update_preference(&scope, 1, confirm())?;
            admin.update_preference(
                &scope,
                2,
                PreferenceChange {
                    operation_id: "correct-style".into(),
                    at_ms: 3,
                    evidence: PreferenceEvidence::Statement(UserStatement {
                        evidence_id: "correction-1".into(),
                        message_id: "message-2".into(),
                        text: "请改成先结论，并保留必要细节。".into(),
                        at_ms: 3,
                    }),
                    action: PreferenceAction::Correct {
                        id: "style".into(),
                        text: "先给结论，并保留必要细节".into(),
                    },
                },
            )?;
            admin.update_preference(
                &scope,
                3,
                PreferenceChange {
                    operation_id: "revoke-style".into(),
                    at_ms: 4,
                    evidence: PreferenceEvidence::Statement(UserStatement {
                        evidence_id: "revocation-1".into(),
                        message_id: "message-3".into(),
                        text: "撤销这条偏好。".into(),
                        at_ms: 4,
                    }),
                    action: PreferenceAction::Revoke { id: "style".into() },
                },
            )?;
        }
        let saved = reader.snapshot()?;
        if saved.revision != 4 || saved.evidence.len() != 3 || saved.preferences.len() != 1 {
            return Err("恢复的交互证据或偏好数量不符".into());
        }
        let preference = &saved.preferences[0];
        if preference.status != PreferenceStatus::Revoked
            || preference.history.len() != 3
            || preference.history[0].text != "先给结论"
            || preference.text != "先给结论，并保留必要细节"
        {
            return Err("更正、撤销或原始历史未完整保存".into());
        }
        let completed = sessions.snapshot(&key)?.ok_or("重启后的会话缺失")?;
        if completed.turns.len() != 1 {
            return Err("重启重复执行了原始轮次".into());
        }
        if admin.import_completed(
            &scope,
            0,
            CompletedInteraction {
                evidence_id: "interaction-1".into(),
                message_id: "message-1".into(),
                at_ms: 99,
                snapshot: completed,
                turn_id: 1,
            },
        )? != saved
            || admin.update_preference(&scope, 0, confirm())? != saved
        {
            return Err("重放改写了已保存记忆".into());
        }
        let other = MemoryScope {
            user_id: "other-user".into(),
            ..scope.clone()
        };
        if !admin.reader(other)?.snapshot()?.evidence.is_empty() {
            return Err("不同用户读取到了其他用户的证据".into());
        }
        if before.as_ref().is_some_and(|bytes| {
            store.get(&owner, MEMORY_STATE_KEY).ok().flatten().as_ref() != Some(bytes)
        }) {
            return Err("重启改写了持久化原文".into());
        }
        Ok::<_, Box<dyn Error>>((reader, scope, saved, restored_revision))
    }
    .await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    let (reader, scope, saved, restored_revision) = result?;
    stopped?;
    flushed?;
    if reader.snapshot() != Err(MemoryError::Unavailable)
        || !matches!(admin.reader(scope), Err(MemoryError::Unavailable))
    {
        return Err("停止后旧句柄仍有效".into());
    }
    let committed = store
        .get(&owner, MEMORY_STATE_KEY)?
        .ok_or("记忆持久状态丢失")?;
    drop(kernel);
    drop(registry);
    drop(store);
    let reopened = FileStateStore::open(&directory)?;
    if reopened.get(&owner, MEMORY_STATE_KEY)? != Some(committed) {
        return Err("重新打开状态目录后字节不符".into());
    }
    println!(
        "{}",
        json!({
            "restored_revision": restored_revision, "revision": saved.revision,
            "evidence": saved.evidence.len(), "preference_history": saved.preferences[0].history.len(),
            "revoked": true, "old_handles_closed": true, "directory_reopened": true,
            "model_requests": 0, "tool_executions": 0,
        })
    );
    Ok(())
}
