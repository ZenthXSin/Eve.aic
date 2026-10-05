//! 对同一独立目录连续运行三次：保存候选和未完成批次、恢复中断、只读复核。
//! 使用真实 Kernel 与 FileStateStore；不调用模型、工具或发送进程信号。
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_learning_api::*;
use eve_learning_plugin::LearningPlugin;
use eve_memory_api::{EvidenceSource, InteractionEvidence, MemoryScope, MemorySnapshot};
use eve_plugin_api::{PluginId, StateStore};
use serde_json::{Value, json};
use std::{error::Error, path::PathBuf, sync::Arc};

fn source() -> MemorySnapshot {
    MemorySnapshot {
        scope: MemoryScope {
            channel: "example".into(),
            session_id: "learning-recovery-session".into(),
            user_id: "learning-recovery-user".into(),
        },
        revision: 6,
        evidence: (1..=6)
            .map(|number| InteractionEvidence {
                id: format!("interaction-{number}"),
                revision: number,
                at_ms: number,
                source: EvidenceSource::CompletedInteraction {
                    message_id: format!("message-{number}"),
                    session_revision: number * 2,
                    turn_id: number,
                    user_text: format!("第 {number} 次：请先说结论。"),
                    assistant_text: format!("第 {number} 次已送达的结论。"),
                },
            })
            .collect(),
        preferences: vec![],
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
    let owner = PluginId::new(LEARNING_PLUGIN_ID)?;
    let before = store.get(&owner, LEARNING_STATE_KEY)?;
    let had_running = before.as_deref().is_some_and(|bytes| {
        serde_json::from_slice::<Value>(bytes)
            .ok()
            .and_then(|value| {
                value["jobs"].as_array().map(|jobs| {
                    jobs.iter()
                        .any(|record| record["job"]["status"] == "Running")
                })
            })
            .unwrap_or(false)
    });
    let kernel = Kernel::with_services(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    });
    let plugin = LearningPlugin::new()?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    let source = source();
    let result = async {
        kernel.start_all().await?;
        if before.is_none() {
            let mut initial = source.clone();
            initial.revision = 3;
            initial.evidence.truncate(3);
            let first = admin
                .reserve(
                    &initial,
                    100,
                    "recovery-extractor-v1",
                    &LearningOptions::default(),
                )?
                .ok_or("首次批次没有保存")?;
            admin.finish(
                &first,
                110,
                LearningOutcome::Completed(vec![CandidateDraft {
                    text: "用户可能偏好先看结论，等待明确确认".into(),
                    confidence: 70,
                    evidence_ids: first.evidence.iter().map(|item| item.id.clone()).collect(),
                }]),
            )?;
            admin
                .reserve(
                    &source,
                    300_100,
                    "recovery-extractor-v1",
                    &LearningOptions::default(),
                )?
                .ok_or("未完成批次没有保存")?;
        }
        let snapshot = admin.snapshot(&source.scope)?;
        if snapshot.jobs.len() != 2
            || snapshot.jobs[0].status != JobStatus::Completed
            || snapshot.jobs[0].candidates.len() != 1
            || snapshot.jobs[0].batch.evidence != source.evidence[..3]
            || snapshot.jobs[1].batch.evidence != source.evidence[3..]
            || !snapshot.jobs[1].candidates.is_empty()
            || snapshot.jobs[1].finished_at_ms.is_some()
        {
            return Err("候选、输入来源或未完成批次恢复不符".into());
        }
        let expected = if before.is_none() {
            JobStatus::Running
        } else {
            JobStatus::Interrupted
        };
        if snapshot.jobs[1].status != expected {
            return Err("重启没有把未完成批次保存为 Interrupted".into());
        }
        let candidate = &snapshot.jobs[0].candidates[0];
        if candidate.created_at_ms != 110
            || candidate.expires_at_ms != 110 + CANDIDATE_TTL_MS
            || candidate.batch_id != snapshot.jobs[0].batch.id
        {
            return Err("候选的来源或有效期被改写".into());
        }
        if admin
            .reserve(
                &source,
                600_100,
                "recovery-extractor-v2",
                &LearningOptions::default(),
            )?
            .is_some()
        {
            return Err("恢复重复消费了已完成或未完成的证据".into());
        }
        admin.finish(
            &snapshot.jobs[0].batch,
            900_100,
            LearningOutcome::Completed(vec![candidate.draft.clone()]),
        )?;
        if admin.snapshot(&source.scope)? != snapshot {
            return Err("幂等重放改写了候选或历史".into());
        }
        let other = MemoryScope {
            user_id: "other-user".into(),
            ..source.scope.clone()
        };
        if !admin.snapshot(&other)?.jobs.is_empty() {
            return Err("不同用户读取到其他用户的批次".into());
        }
        let committed = store
            .get(&owner, LEARNING_STATE_KEY)?
            .ok_or("持久状态缺失")?;
        if let Some(previous) = &before {
            if had_running {
                let mut expected: Value = serde_json::from_slice(previous)?;
                for record in expected["jobs"].as_array_mut().ok_or("批次格式无效")? {
                    if record["job"]["status"] == "Running" {
                        record["job"]["status"] = json!("Interrupted");
                    }
                }
                if serde_json::from_slice::<Value>(&committed)? != expected {
                    return Err("中断恢复修改了结局以外的原始状态".into());
                }
            } else if committed != *previous {
                return Err("只读恢复修改了持久化字节".into());
            }
        }
        Ok::<_, Box<dyn Error>>((snapshot, committed))
    }
    .await;
    let stopped = kernel.stop_all().await;
    let flushed = kernel.flush_logs();
    let (snapshot, committed) = result?;
    stopped?;
    flushed?;
    if admin.snapshot(&source.scope) != Err(LearningError::Unavailable) {
        return Err("停止后旧句柄仍有效".into());
    }
    drop(kernel);
    drop(store);
    let reopened = FileStateStore::open(&directory)?;
    if reopened.get(&owner, LEARNING_STATE_KEY)? != Some(committed) {
        return Err("重新打开目录后持久字节不符".into());
    }
    println!(
        "{}",
        json!({
            "jobs": snapshot.jobs.len(),
            "candidates": snapshot.jobs[0].candidates.len(),
            "interrupted": snapshot.jobs[1].status == JobStatus::Interrupted,
            "recovered_running": had_running,
            "read_only_restart": before.is_some() && !had_running,
            "old_handles_closed": true,
            "directory_reopened": true,
            "model_requests": 0,
            "tool_executions": 0,
        })
    );
    Ok(())
}
