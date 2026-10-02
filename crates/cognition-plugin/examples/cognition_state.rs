//! 连续三次独立运行：状态夹具保存、在途目标阻塞恢复、无变化恢复。
//! 不调用模型或工具；完成目标的证据由受信宿主夹具提供。
use eve_cognition_api::*;
use eve_cognition_plugin::{COGNITION_STATE_KEY, CognitionPlugin};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_plugin_api::{PluginId, ServiceId, StateStore};
use serde_json::json;
use std::{error::Error, path::PathBuf, sync::Arc};

fn goal(id: &str, visibility: Visibility) -> Goal {
    Goal {
        id: id.into(),
        revision: 0,
        source: Source {
            kind: SourceKind::Internal,
            channel: "internal".into(),
            reference: "fixture".into(),
        },
        visibility,
        description: "保存可恢复的本地目标".into(),
        verification: "检查状态机回执".into(),
        priority: 50,
        budget: ExecutionBudget {
            max_model_requests: 4,
            max_tool_calls: 1,
            max_attempts: 1,
            timeout_ms: 1000,
        },
        stop_condition: "保存反馈后结束".into(),
        expires_at_ms: None,
        status: GoalStatus::Ready,
        wait_reason: None,
        block_reason: None,
        execution: None,
        feedback: None,
    }
}
fn execute(goal: &mut Goal, turn_id: Option<u64>) {
    goal.status = GoalStatus::Executing;
    goal.execution = Some(ExecutionAttempt {
        attempt_id: goal.id.clone(),
        session_id: "fixture-session".into(),
        task_id: goal.id.clone(),
        turn_id,
        started_at_ms: 1,
    });
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let directory = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("用法：cognition_state <状态目录>")?;
    let store = Arc::new(FileStateStore::open(&directory)?);
    let persisted = store
        .get(&PluginId::new(COGNITION_PLUGIN_ID)?, COGNITION_STATE_KEY)?
        .map(|bytes| serde_json::from_slice::<CognitiveSnapshot>(&bytes))
        .transpose()?;
    let restored_revision = persisted.as_ref().map_or(0, |s| s.revision);
    let services = KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    };
    let registry = services.registry.clone();
    let kernel = Kernel::with_services(services);
    let plugin = CognitionPlugin::new("eve")?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start_all().await?;
    let snapshot = admin.snapshot()?;
    if snapshot.revision == 0 {
        let mut state = CognitiveState::default();
        state
            .goals
            .insert("ready".into(), goal("ready", Visibility::Public));
        state
            .goals
            .insert("done".into(), goal("done", Visibility::Internal));
        state
            .goals
            .insert("uncertain".into(), goal("uncertain", Visibility::Internal));
        state.goals.get_mut("ready").unwrap().source.channel = "terminal".into();
        state.goals.get_mut("ready").unwrap().source.kind = SourceKind::User;
        let mut waiting = goal("waiting", Visibility::User("fixture-user".into()));
        waiting.source.channel = "qq".into();
        waiting.source.kind = SourceKind::User;
        waiting.status = GoalStatus::Waiting;
        waiting.wait_reason = Some("等待用户补充".into());
        state.goals.insert("waiting".into(), waiting);
        state.drives.insert(
            "unfinished".into(),
            Drive {
                id: "unfinished".into(),
                visibility: Visibility::User("fixture-user".into()),
                goal_ids: vec!["waiting".into()],
                strength: 50,
                reason: "保留未完成目标".into(),
                evaluated_at_ms: 1,
                valid_until_ms: 100,
            },
        );
        state.events.push(CognitiveEvent {
            id: "created".into(),
            kind: CognitiveEventKind::StateChanged,
            source: state.goals["ready"].source.clone(),
            visibility: Visibility::Public,
            goal_id: Some("ready".into()),
            caused_by: None,
            at_ms: 1,
            summary: "创建本地目标".into(),
        });
        let mut state = admin.replace(0, state)?.state;
        execute(state.goals.get_mut("done").unwrap(), Some(1));
        let mut state = admin.replace(1, state)?.state;
        let done = state.goals.get_mut("done").unwrap();
        done.status = GoalStatus::Completed;
        done.feedback = Some(Feedback {
            commit: ExecutionCommit::Completed,
            verification_met: true,
            started_tools: Some(0),
            summary: "宿主状态机夹具验证完成".into(),
            at_ms: 2,
        });
        let mut state = admin.replace(2, state)?.state;
        execute(state.goals.get_mut("uncertain").unwrap(), None);
        state.agenda = Some(Agenda {
            visibility: Visibility::Internal,
            candidates: vec!["ready".into()],
            selected: Some("uncertain".into()),
            reason: "模拟执行标记后的崩溃窗口".into(),
            valid_until_ms: 100,
        });
        admin.replace(3, state)?;
    }
    let snapshot = admin.snapshot()?;
    let public = registry
        .get(&ServiceId::new(COGNITION_READ_SERVICE_ID)?)?
        .ok_or("认知公开服务未发布")?
        .value
        .downcast::<CognitionReadHandle>()
        .map_err(|_| "认知公开服务类型错误")?
        .0
        .clone();
    let user = admin.reader(ReadAccess::User("fixture-user".into()))?;
    let internal = admin.reader(ReadAccess::Internal)?;
    let public_view = public.snapshot()?;
    let user_view = user.snapshot()?;
    let internal_view = internal.snapshot()?;
    if public_view.state.goals.len() != 1
        || user_view.state.goals.len() != 2
        || internal_view.state.goals.len() != 4
        || user_view.state.drives.len() != 1
        || public_view.state.agenda.is_some()
        || snapshot.state.goals["done"].status != GoalStatus::Completed
    {
        return Err("认知读权限、来源或终态恢复不符".into());
    }
    let blocked = snapshot
        .state
        .goals
        .values()
        .filter(|g| g.status == GoalStatus::Blocked)
        .count();
    if restored_revision > 0 {
        if snapshot.state.goals["uncertain"].block_reason != Some(BlockReason::Interrupted)
            || snapshot
                .state
                .agenda
                .as_ref()
                .is_some_and(|a| a.selected.is_some())
        {
            return Err("在途恢复没有阻塞或仍被选中".into());
        }
        for id in ["ready", "done", "waiting"] {
            if snapshot.state.goals[id] != persisted.as_ref().unwrap().state.goals[id] {
                return Err("恢复改写了已保存目标".into());
            }
        }
    }
    kernel.stop_all().await?;
    for reader in [public, user, internal] {
        if reader.snapshot() != Err(CognitionError::Unavailable) {
            return Err("旧读句柄仍有效".into());
        }
    }
    if admin.snapshot() != Err(CognitionError::Unavailable) {
        return Err("旧管理句柄仍有效".into());
    }
    drop(kernel);
    drop(store);
    let reopened = FileStateStore::open(&directory)?;
    let disk: CognitiveSnapshot = serde_json::from_slice(
        &reopened
            .get(&PluginId::new(COGNITION_PLUGIN_ID)?, COGNITION_STATE_KEY)?
            .ok_or("认知状态丢失")?,
    )?;
    if disk != snapshot {
        return Err("磁盘与认知快照不一致".into());
    }
    println!(
        "{}",
        json!({
            "restored_revision": restored_revision, "revision": snapshot.revision,
            "goals": snapshot.state.goals.len(), "public_goals": public_view.state.goals.len(),
            "user_goals": user_view.state.goals.len(), "ready_goals": internal_view.ready_goal_ids(2).len(),
            "completed_goals": 1, "waiting_goals": 1, "blocked_goals": blocked,
            "closed_services": true, "directory_reopened": true, "model_requests": 0, "tool_executions": 0,
        })
    );
    Ok(())
}
