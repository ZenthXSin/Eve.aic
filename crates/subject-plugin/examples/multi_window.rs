//! 本地确定性模拟：两个窗口同时派发、后台完成回流、三十分钟检查、未知副作用恢复。
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::PluginId;
use eve_subject_api::*;
use eve_subject_plugin::{SubjectDispatch, SubjectDriver, SubjectPlugin};
use std::sync::Arc;

fn scope(window: &str, user: &str) -> SubjectScope {
    SubjectScope {
        channel: "qq".into(),
        conversation_id: window.into(),
        principal_id: user.into(),
    }
}
fn input(scope: SubjectScope, key: &str, priority: EventPriority) -> SubjectEventInput {
    SubjectEventInput {
        scope,
        source: match priority {
            EventPriority::Stop | EventPriority::User => EventSource::UserIngress,
            EventPriority::Recovery => EventSource::TaskExecutor,
            EventPriority::Autonomy => EventSource::AutonomyScheduler,
        },
        dedupe_key: key.into(),
        priority,
        kind: "simulation".into(),
        payload: "仅供本范围的任务说明".into(),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(MemoryStateStore::default());
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = SubjectPlugin::new()?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    let plugin_id = PluginId::new(SUBJECT_PLUGIN_ID)?;
    kernel.start(&plugin_id).await?;
    let driver = SubjectDriver::new(Arc::new(admin.clone()), "simulation")?;
    let t = 1_000;
    let alice = scope("private-a", "alice");
    let bob = scope("group-b", "bob");
    admin.enqueue(input(alice.clone(), "a-message", EventPriority::User), t)?;
    admin.enqueue(input(bob.clone(), "b-message", EventPriority::User), t)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..2 {
        let tx = tx.clone();
        driver.run_one(t, |event| {
            let task_id = format!("task-{}", event.id);
            let task_id_for_result = task_id.clone();
            let scope = event.input.scope.clone();
            tokio::spawn(async move {
                let _ = tx.send((scope, task_id_for_result));
            });
            Ok(SubjectDispatch::Started {
                task_id,
                title: "独立后台任务".into(),
                goal: "依据原范围要求持续推进".into(),
            })
        })?;
    }
    println!("两个窗口均已派发，主体未持有租约等待后台执行");
    for _ in 0..2 {
        let (scope, task_id) = rx.recv().await.ok_or("后台回执通道关闭")?;
        admin.enqueue(
            input(
                scope,
                &format!("completed:{task_id}"),
                EventPriority::Recovery,
            ),
            t + 1,
        )?;
        driver.run_one(t + 1, |_| {
            Ok(SubjectDispatch::Completed {
                summary: "已在原范围处理完成回执".into(),
            })
        })?;
    }
    assert_eq!(admin.snapshot_for(&alice)?.events.len(), 2);
    assert_eq!(admin.snapshot_for(&bob)?.events.len(), 2);
    assert_eq!(admin.schedule_due(t + AUTONOMY_INTERVAL_MS - 1)?, 0);
    assert_eq!(admin.schedule_due(t + AUTONOMY_INTERVAL_MS)?, 2);
    println!("后台回执按范围隔离；半小时只产生检查事件，不发送机械通知");
    // Stop 到达后应优先于已排队的两条自主检查。
    admin.enqueue(
        input(alice.clone(), "stop", EventPriority::Stop),
        t + AUTONOMY_INTERVAL_MS,
    )?;
    driver.run_one(t + AUTONOMY_INTERVAL_MS, |event| {
        assert_eq!(event.input.priority, EventPriority::Stop);
        admin.cancel_scope(&event.input.scope, t + AUTONOMY_INTERVAL_MS)?;
        Ok(SubjectDispatch::Cancelled)
    })?;
    println!("停止指令先处理，只取消对应范围的关注和排队工作");
    kernel.stop(&plugin_id).await?;
    println!("本地模拟验收完成；未连接QQ、未发送外部消息");
    Ok(())
}
