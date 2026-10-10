use crate::{Services, failed, prefix, value, view};
use eve_cognition_api::{GoalStatus, Visibility};
use eve_llm_api::{ToolExecutionContext, ToolExecutionError, ToolValidationError};
use eve_practice_api::{
    AttemptStage, PracticeDraft, PracticeFailure, PracticeTask, validate_artifact, validate_draft,
};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .max(1)
}
pub(crate) fn draft_schema(run: bool) -> Value {
    let draft = json!({
        "type":"object","additionalProperties":false,
        "properties":{
            "applicable":{"type":"boolean"},
            "files":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false}},
            "probes":{"type":"array","items":{"type":"object","properties":{"subject":{"type":"string"},"property":{"type":"string"},"expected":{"type":"string"}},"required":["subject","property","expected"],"additionalProperties":false}},
            "rationale":{"type":"string"},"notes_used":{"type":"array","items":{"type":"string"}}
        },
        "required":["applicable","files","probes","rationale","notes_used"]
    });
    if run {
        json!({"type":"object","properties":{"goal_id":{"type":"string"},"goal_revision":{"type":"integer","minimum":1},"draft":draft},"required":["goal_id","goal_revision","draft"],"additionalProperties":false})
    } else {
        json!({"type":"object","properties":{"draft":draft},"required":["draft"],"additionalProperties":false})
    }
}
pub(crate) fn validate_arguments(value: &Value, run: bool) -> Result<(), ToolValidationError> {
    let invalid = || ToolValidationError {
        message: "需要规范数据文件草稿；执行时还需要有效 goal_id 和 goal_revision，不接受额外字段"
            .into(),
    };
    let object = value.as_object().ok_or_else(invalid)?;
    let required: &[&str] = if run {
        &["goal_id", "goal_revision", "draft"]
    } else {
        &["draft"]
    };
    if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
        return Err(invalid());
    }
    if run
        && (value["goal_id"]
            .as_str()
            .is_none_or(|id| eve_practice_api::validate_id(id).is_err())
            || value["goal_revision"].as_u64().is_none_or(|r| r == 0))
    {
        return Err(invalid());
    }
    let _: PracticeDraft = serde_json::from_value(value["draft"].clone()).map_err(|_| invalid())?;
    Ok(())
}
fn runner(s: &Services) -> Result<Arc<dyn eve_practice_api::PracticeRunner>, ToolExecutionError> {
    s.runner
        .as_ref()
        .and_then(|slot| slot.get())
        .cloned()
        .ok_or_else(|| failed("实际运行器尚未就绪"))
}
pub(crate) fn check(
    s: &Services,
    owner: &str,
    arguments: &Value,
) -> Result<Value, ToolExecutionError> {
    let runner = runner(s)?;
    let draft: PracticeDraft =
        serde_json::from_value(arguments["draft"].clone()).map_err(|_| failed("草稿无效"))?;
    validate_artifact(runner.profile(), &draft)
        .map_err(|_| failed("草稿结构无效或超出数据文件范围"))?;
    let mut issues = runner.check(&draft);
    if let Some(checks) = &s.checks {
        let snapshot = checks
            .snapshot()
            .map_err(|_| failed("锻造规则暂时不可用"))?;
        for (_, v) in snapshot.enabled_for(owner, &runner.profile().runner_id) {
            issues.extend(eve_toolforge_api::evaluate(&v.spec, &draft.files));
        }
    }
    Ok(
        json!({"preflight_passed":issues.is_empty(),"verified":false,"issues":issues,"runner":runner.profile(),"note":"这是结构预检；尚未实际运行，不代表验证通过。"}),
    )
}
fn current_goal(
    s: &Services,
    owner: &str,
    id: &str,
    revision: u64,
) -> Result<eve_cognition_api::Goal, ToolExecutionError> {
    let snapshot = s
        .cognition
        .as_ref()
        .and_then(|slot| slot.get())
        .ok_or_else(|| failed("认知能力尚未就绪"))?
        .snapshot()
        .map_err(|_| failed("目标读取失败"))?;
    let goal = snapshot
        .state
        .goals
        .get(id)
        .filter(|g| g.visibility == Visibility::User(owner.into()))
        .ok_or_else(|| failed("目标不存在或不属于当前用户"))?;
    if goal.revision != revision {
        return Err(failed("目标修订已变化，请先重新读取目标"));
    }
    if !matches!(goal.status, GoalStatus::Waiting | GoalStatus::Ready) {
        return Err(failed("目标当前不接受实践；未开始运行"));
    }
    Ok(goal.clone())
}
pub(crate) async fn run(
    s: &Services,
    owner: &str,
    arguments: Value,
    context: ToolExecutionContext,
) -> Result<Value, ToolExecutionError> {
    let id = arguments["goal_id"].as_str().unwrap_or_default();
    let revision = arguments["goal_revision"].as_u64().unwrap_or_default();
    let goal = current_goal(s, owner, id, revision)?;
    let runner = runner(s)?;
    let admin = s
        .practice
        .as_ref()
        .ok_or_else(|| failed("实践能力未启用"))?;
    // 与后台共享准入账本：同一修订已有记录时读回，不重跑或争抢进行中的任务。
    let snapshot = admin.snapshot().map_err(|_| failed("实践记录读取失败"))?;
    if let Some(existing) = snapshot
        .runs
        .iter()
        .find(|r| r.task.owner == owner && r.task.goal_id == id && r.task.goal_revision == revision)
    {
        return Ok(
            json!({"started":false,"replayed":false,"existing":view(vec![value(existing)?])?}),
        );
    }
    let draft: PracticeDraft =
        serde_json::from_value(arguments["draft"].clone()).map_err(|_| failed("草稿无效"))?;
    let brief = prefix(&goal.description, eve_practice_api::MAX_BRIEF_BYTES);
    let task = PracticeTask {
        goal_id: id.into(),
        goal_revision: revision,
        owner: owner.into(),
        brief: brief.into(),
        brief_truncated: brief.len() != goal.description.len(),
        notes: vec![],
    };
    validate_draft(&task, runner.profile(), &draft)
        .map_err(|_| failed("草稿结构或资料引用无效；未保存准入，未运行"))?;
    if context.is_cancel_requested() {
        return Err(ToolExecutionError::Cancelled("轮次已取消，未运行".into()));
    }
    let mut run = admin
        .begin(task, runner.profile(), "eve.dialogue-tools-1", now_ms())
        .map_err(|_| failed("实践准入保存失败；未运行"))?
        .ok_or_else(|| failed("实践正忙、修订已执行或预算已用尽；未运行"))?;
    let mut issues = runner.check(&draft);
    if let Some(check) = &s.draft_check {
        match check.check(&run, &draft, now_ms()) {
            Ok(additional) => issues.extend(additional),
            Err(_) => {
                admin
                    .abandon(&run.id, now_ms(), PracticeFailure::InvalidOutput)
                    .map_err(|_| failed("预检失败，结局保存失败；保留状态"))?;
                return Err(failed("锻造检查失败；未运行"));
            }
        }
    }
    run = admin
        .record_draft(&run.id, now_ms(), Ok(draft.clone()), issues.clone())
        .map_err(|_| failed("草稿保存失败；未运行"))?;
    if run.current().is_none_or(|attempt| {
        attempt.stage != AttemptStage::Running || attempt.finished_at_ms.is_some()
    }) {
        if run.finished_at_ms.is_none() {
            run = admin
                .abandon(&run.id, now_ms(), PracticeFailure::InvalidOutput)
                .map_err(|_| failed("拒绝结局保存失败；保留状态"))?;
        }
        return Ok(
            json!({"started":false,"verified":false,"issues":issues,"record":view(vec![value(run)?])?}),
        );
    }
    if context.is_cancel_requested() || current_goal(s, owner, id, revision).is_err() {
        admin
            .abandon(&run.id, now_ms(), PracticeFailure::Cancelled)
            .map_err(|_| failed("取消保存失败；保留状态"))?;
        return Err(ToolExecutionError::Cancelled(
            "当前目标或轮次已失效；未运行".into(),
        ));
    }
    let workspace = s.workspace_root.join(&run.id);
    // 工作路径完全由宿主及账本稳定 ID 构造，拒绝已有目录，保留可疑旧文件。
    if std::fs::create_dir_all(&s.workspace_root).is_err()
        || std::fs::create_dir(&workspace).is_err()
    {
        admin
            .abandon(&run.id, now_ms(), PracticeFailure::InvalidOutput)
            .map_err(|_| failed("目录失败的结局保存失败"))?;
        return Err(failed("无法创建全新实践目录；保留已有文件，未运行"));
    }
    let cancellation = async {
        loop {
            if context.is_cancel_requested() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let evidence = tokio::select! {
        biased;
        _ = cancellation => Err(PracticeFailure::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(120), runner.run(&draft,&workspace)) => result.map_err(|_| PracticeFailure::Timeout),
    };
    let _ = std::fs::remove_dir_all(&workspace);
    match evidence {
        Ok(evidence) => {
            let verified = evidence.verified(&draft);
            run = admin
                .record_evidence(&run.id, now_ms(), evidence)
                .map_err(|_| failed("实际运行完成，但证据保存失败；保留状态，不能声称已固化"))?;
            // 一次对话调用只执行一次实际运行；失败留存，不留下等待自动重放的下一次草稿。
            if run.finished_at_ms.is_none() {
                run = admin
                    .abandon(&run.id, now_ms(), PracticeFailure::InvalidOutput)
                    .map_err(|_| failed("失败结局保存失败"))?;
            }
            Ok(
                json!({"started":true,"replayed":false,"verified":verified,"record":view(vec![value(run)?])?}),
            )
        }
        Err(failure) => {
            admin
                .abandon(&run.id, now_ms(), failure)
                .map_err(|_| failed("中断结局保存失败；保留状态，不重放"))?;
            Err(if failure == PracticeFailure::Cancelled {
                ToolExecutionError::Cancelled("实践已取消并留下记录".into())
            } else {
                failed("实践超时并留下记录；未重试")
            })
        }
    }
}
