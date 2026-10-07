//! 有条件多步计划：本地操作者提供，或由模型建议后经操作者确认；绑定当前目标修订与输入，
//! 按依赖逐步执行宿主登记的能力。
//!
//! 计划只引用能力 ID；路径由每次命令显式绑定，不进入计划或状态。步骤效果只按宿主独立读取的
//! 证据判定。模型建议不授予执行权限。计划完成不改变父目标，父目标仍保持 Waiting。
use crate::{
    AppError,
    cognition::{interrupted, plan_model_resolver},
    cognition_action::{self, ExportPlanOptions},
    cognition_action_admission::{current_plan_binding, plan_context},
};
use eve_action_api::{ActionFailure, ActionStatus, MAX_ACTION_TIMEOUT_MS};
use eve_cognition_plugin::CognitionController;
use eve_file_observer::BoundFileObserver;
use eve_kernel::Kernel;
use eve_plan_api::*;
use eve_plan_plugin::{ModelPlanProposer, PlanController, PlanPlugin};
use eve_plugin_api::{PluginId, ServiceId, ServiceRegistry};
use eve_session_api::{SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// 重新读取宿主显式绑定的文本文件，得到完整字节摘要；不修改认知目标或输入证据。
pub(crate) const OBSERVE: &str = "eve.file.observe.v1";
/// 以受控行动导出当前目标修订已完成的反思草稿到新文件，并独立回读。
pub(crate) const EXPORT: &str = "eve.artifact.export.v1";
const SUBJECT: &str = "eve";

/// 宿主登记的能力上限。导出有外部副作用，只允许一次尝试，且须绑定输入摘要。
pub(crate) fn capabilities() -> Vec<CapabilitySpec> {
    vec![
        CapabilitySpec {
            id: OBSERVE.into(),
            description: "重新读取操作者在执行时绑定的文本文件，得到完整字节 SHA-256；只读，不修改目标或输入证据。".into(),
            max_attempts: MAX_STEP_ATTEMPTS,
            max_timeout_ms: MAX_STEP_TIMEOUT_MS,
            requires_input: false,
        },
        CapabilitySpec {
            id: EXPORT.into(),
            description: "把当前目标修订已验证的反思草稿导出到操作者指定的新文件并独立回读；有外部副作用，timeout_ms 须为 30000。".into(),
            max_attempts: 1,
            max_timeout_ms: MAX_ACTION_TIMEOUT_MS,
            requires_input: true,
        },
    ]
}

#[derive(Clone, Debug)]
pub(crate) struct PlanCreateOptions {
    pub goal_id: String,
    pub goal_revision: u64,
    pub user_id: String,
    pub input_sha256: Option<String>,
    pub steps_file: PathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct PlanProposeOptions {
    pub goal_id: String,
    pub goal_revision: u64,
    pub user_id: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PlanStepOptions {
    pub plan_id: String,
    pub step_id: String,
    pub user_id: String,
    pub observe_file: Option<PathBuf>,
    pub output_file: Option<PathBuf>,
}

/// 步骤文件只含步骤；绑定由命令参数给出，并在保存前与当前状态核对。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepsFile {
    steps: Vec<StepSpec>,
}

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

async fn open_plans(kernel: &Kernel) -> Result<PlanController, AppError> {
    let plugin = PlanPlugin::new(SUBJECT)?;
    let controller = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&PluginId::new(PLAN_PLUGIN_ID)?).await?;
    Ok(controller)
}

fn plan_view(plan: &Plan) -> Value {
    json!({"plan": plan, "ready_steps": plan.ready_steps()})
}

/// 当前状态下该计划应有的绑定；只依赖输入摘要的计划才比较输入。
fn current_binding(
    admin: &CognitionController,
    plan: &Plan,
    user_id: &str,
    at_ms: u64,
) -> Result<PlanBinding, AppError> {
    let (revision, input) = current_plan_binding(admin, &plan.binding.goal_id, user_id, at_ms)?;
    Ok(PlanBinding {
        goal_id: plan.binding.goal_id.clone(),
        goal_revision: revision,
        input_sha256: plan.binding.input_sha256.as_ref().and(input),
    })
}

/// 同一目标的待确认或活动计划若已过时则封存，使新计划不会被旧绑定挡住。
fn reconcile_goal(
    journal: &PlanController,
    admin: &CognitionController,
    goal_id: &str,
    user_id: &str,
    at_ms: u64,
) -> Result<(), AppError> {
    for plan in journal.snapshot()?.plans {
        if plan.is_open() && plan.binding.goal_id == goal_id {
            let current = current_binding(admin, &plan, user_id, at_ms)?;
            journal.invalidate(&plan.id, plan.revision, &current, at_ms)?;
        }
    }
    Ok(())
}

pub(crate) async fn create(
    kernel: &Kernel,
    admin: &CognitionController,
    options: PlanCreateOptions,
) -> Result<Value, AppError> {
    let metadata = std::fs::metadata(&options.steps_file)?;
    if !metadata.is_file() || metadata.len() > MAX_PLAN_JSON_BYTES as u64 {
        return Err("步骤文件须为不超过 16 KiB 的普通文件。".into());
    }
    let text = String::from_utf8(std::fs::read(&options.steps_file)?)
        .map_err(|_| "步骤文件必须为 UTF-8 JSON。")?;
    let steps: StepsFile = serde_json::from_str(&text).map_err(|_| PlanError::InvalidInput)?;
    if !export_timeouts_fixed(&steps.steps) {
        return Err("导出步骤沿用受控行动 30 秒期限，timeout_ms 须为 30000。".into());
    }
    let at_ms = now_ms()?;
    let (revision, input) = current_plan_binding(admin, &options.goal_id, &options.user_id, at_ms)?;
    if revision != options.goal_revision {
        return Err("目标修订已变化，请查看当前修订后再建立计划。".into());
    }
    if options.input_sha256.is_some() && options.input_sha256 != input {
        return Err("输入摘要与目标最近一次文件观察不一致，未建立计划。".into());
    }
    let spec = PlanSpec {
        binding: PlanBinding {
            goal_id: options.goal_id.clone(),
            goal_revision: revision,
            input_sha256: options.input_sha256,
        },
        steps: steps.steps,
    };
    spec.validate(&capabilities())?;
    let journal = open_plans(kernel).await?;
    reconcile_goal(&journal, admin, &options.goal_id, &options.user_id, at_ms)?;
    let created = journal.create(spec, &capabilities(), at_ms)?;
    Ok(
        json!({"command": "plan-create", "duplicate": created.duplicate,
        "plan": plan_view(&created.plan)}),
    )
}

/// 导出步骤使用受控行动固定的期限，计划不能声明更短的期限后被忽略。
fn export_timeouts_fixed(steps: &[StepSpec]) -> bool {
    steps
        .iter()
        .all(|step| step.capability != EXPORT || step.timeout_ms == MAX_ACTION_TIMEOUT_MS)
}

fn proposal_view(record: &ProposalRecord, plan: Option<&Plan>) -> Value {
    json!({"proposal": record, "plan": plan.map(plan_view)})
}

async fn open_sessions(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
) -> Result<Arc<SessionServiceHandle>, AppError> {
    kernel.register(Box::new(SessionPlugin::new()?))?;
    kernel.start(&PluginId::new(SESSION_PLUGIN_ID)?).await?;
    Ok(registry
        .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
        .ok_or("会话服务缺失。")?
        .value
        .downcast::<SessionServiceHandle>()
        .map_err(|_| "会话服务类型错误。")?)
}

/// 请模型为当前目标修订建议一份计划。每个绑定（目标修订与输入摘要）至多保存一条请求记录、
/// 发起一次无工具请求；已有记录时直接返回，不再请求。建议校验通过后保存为待确认计划。
pub(crate) async fn propose(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
    admin: &CognitionController,
    state_directory: &Path,
    agent_path: &Path,
    options: PlanProposeOptions,
) -> Result<Value, AppError> {
    let at_ms = now_ms()?;
    let (revision, input) = current_plan_binding(admin, &options.goal_id, &options.user_id, at_ms)?;
    if revision != options.goal_revision {
        return Err("目标修订已变化，请查看当前修订后再请求计划建议。".into());
    }
    let binding = PlanBinding {
        goal_id: options.goal_id.clone(),
        goal_revision: revision,
        input_sha256: input,
    };
    let journal = open_plans(kernel).await?;
    reconcile_goal(&journal, admin, &options.goal_id, &options.user_id, at_ms)?;
    let existing = |journal: &PlanController| -> Result<Option<Value>, AppError> {
        let snapshot = journal.snapshot()?;
        let id = proposal_id(SUBJECT, &binding)?;
        Ok(snapshot
            .proposals
            .iter()
            .find(|record| record.id == id)
            .map(|record| {
                let plan = match &record.status {
                    ProposalStatus::Proposed { plan_id } => {
                        snapshot.plans.iter().find(|plan| &plan.id == plan_id)
                    }
                    _ => None,
                };
                proposal_view(record, plan)
            }))
    };
    if let Some(view) = existing(&journal)? {
        return Ok(json!({"command": "plan-propose", "duplicate": true,
            "model_request_attempted": false, "result": view}));
    }
    // 上下文与模型配置在保存请求记录之前准备；这些失败不消耗该修订的建议机会。
    let sessions = open_sessions(kernel, registry).await?;
    let (goal, draft) = plan_context(
        admin,
        sessions.0.as_ref(),
        &options.goal_id,
        &options.user_id,
        at_ms,
    )?;
    let request = ProposalRequest::new(
        SUBJECT,
        binding.clone(),
        goal,
        draft.map(|artifact| ProposalDraft {
            summary: artifact.summary,
            next_step: artifact.next_step,
        }),
        &capabilities(),
    )?;
    let proposer = ModelPlanProposer::new(
        plan_model_resolver(kernel, registry, state_directory, agent_path).await?,
    );
    let reserved = journal.reserve_proposal(&binding, proposer.version(), at_ms)?;
    if reserved.duplicate {
        let view = existing(&journal)?.ok_or(PlanError::CorruptState)?;
        return Ok(json!({"command": "plan-propose", "duplicate": true,
            "model_request_attempted": false, "result": view}));
    }
    let requested = reserved.record.requested_at_ms;
    let result = {
        let stop = interrupted();
        tokio::pin!(stop);
        tokio::select! {
            biased;
            _ = &mut stop => Err(ProposalFailure::Cancelled),
            result = proposer.propose(request) => result,
        }
    };
    let outcome = match result {
        Ok(steps) if steps.is_empty() => ProposalOutcome::Empty,
        Ok(steps) if !export_timeouts_fixed(&steps) => {
            ProposalOutcome::Failed(ProposalFailure::InvalidOutput)
        }
        Ok(steps) => {
            // 请求期间目标或输入变化时，建议不再对应当前目标，不保存为计划。
            let current =
                current_plan_binding(admin, &options.goal_id, &options.user_id, now_ms()?).ok();
            if current != Some((binding.goal_revision, binding.input_sha256.clone())) {
                ProposalOutcome::Failed(ProposalFailure::BindingChanged)
            } else {
                ProposalOutcome::Steps(steps)
            }
        }
        Err(failure) => ProposalOutcome::Failed(failure),
    };
    let finished = journal.finish_proposal(
        &reserved.record.id,
        outcome,
        &capabilities(),
        now_ms()?.max(requested),
    )?;
    Ok(json!({"command": "plan-propose", "duplicate": false,
        "model_request_attempted": true,
        "result": proposal_view(&finished.record, finished.plan.as_ref())}))
}

fn find_plan(journal: &PlanController, plan_id: &str) -> Result<Plan, AppError> {
    Ok(journal
        .snapshot()?
        .plans
        .into_iter()
        .find(|plan| plan.id == plan_id)
        .ok_or(PlanError::NotFound)?)
}

/// 操作者确认待确认的建议。确认前核对目标所有权与当前绑定；已变化时封存为过时，不执行步骤。
pub(crate) async fn confirm(
    kernel: &Kernel,
    admin: &CognitionController,
    plan_id: &str,
    user_id: &str,
) -> Result<Value, AppError> {
    let journal = open_plans(kernel).await?;
    let plan = find_plan(&journal, plan_id)?;
    if plan.status != PlanStatus::Proposed {
        return Err(PlanError::InvalidTransition.into());
    }
    let at_ms = now_ms()?;
    let current = current_binding(admin, &plan, user_id, at_ms)?;
    let plan = journal.confirm(&plan.id, plan.revision, &current, at_ms)?;
    let confirmed = plan.status == PlanStatus::Active;
    let mut report = json!({"command": "plan-confirm", "confirmed": confirmed,
        "plan": plan_view(&plan)});
    if !confirmed {
        report["reason"] = json!("binding_changed");
    }
    Ok(report)
}

/// 操作者撤销待确认或活动计划；只核对目标所有权，不执行任何能力。
pub(crate) async fn withdraw(
    kernel: &Kernel,
    admin: &CognitionController,
    plan_id: &str,
    user_id: &str,
) -> Result<Value, AppError> {
    let journal = open_plans(kernel).await?;
    let plan = find_plan(&journal, plan_id)?;
    let at_ms = now_ms()?;
    current_plan_binding(admin, &plan.binding.goal_id, user_id, at_ms)?;
    let plan = journal.withdraw(&plan.id, plan.revision, at_ms)?;
    Ok(json!({"command": "plan-withdraw", "plan": plan_view(&plan)}))
}

/// 打开账本会先封存遗留 Executing，属于恢复性读取；不执行步骤，不改变认知状态。
pub(crate) async fn show(
    kernel: &Kernel,
    admin: &CognitionController,
    goal_id: Option<String>,
    user_id: &str,
) -> Result<Value, AppError> {
    let journal = open_plans(kernel).await?;
    let at_ms = now_ms()?;
    let snapshot = journal.snapshot()?;
    let plans: Vec<_> = snapshot
        .plans
        .iter()
        .filter(|plan| {
            goal_id
                .as_deref()
                .is_none_or(|id| plan.binding.goal_id == id)
        })
        .map(|plan| {
            // 只报告绑定是否仍成立；封存由 plan-step 或新建计划时执行。
            let current = current_binding(admin, plan, user_id, at_ms)
                .ok()
                .map(|current| current == plan.binding);
            let mut view = plan_view(plan);
            view["binding_current"] = json!(current);
            view
        })
        .collect();
    let proposals: Vec<_> = snapshot
        .proposals
        .iter()
        .filter(|record| {
            goal_id
                .as_deref()
                .is_none_or(|id| record.binding.goal_id == id)
        })
        .collect();
    Ok(json!({"command": "plan-show", "plans": plans, "proposals": proposals}))
}

pub(crate) async fn step(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
    admin: &CognitionController,
    options: PlanStepOptions,
) -> Result<Value, AppError> {
    let journal = open_plans(kernel).await?;
    let plan = journal
        .snapshot()?
        .plans
        .into_iter()
        .find(|plan| plan.id == options.plan_id)
        .ok_or(PlanError::NotFound)?;
    // 已完成、阻塞或过时的计划不再准入步骤；不改动任何状态。
    if plan.status != PlanStatus::Active {
        return Err(PlanError::NotReady.into());
    }
    let at_ms = now_ms()?;
    let current = current_binding(admin, &plan, &options.user_id, at_ms)?;
    if current != plan.binding {
        let plan = journal.invalidate(&plan.id, plan.revision, &current, at_ms)?;
        return Ok(json!({"command": "plan-step", "executed": false,
            "reason": "binding_changed", "plan": plan_view(&plan)}));
    }
    let spec = plan
        .step(&options.step_id)
        .ok_or(PlanError::NotFound)?
        .spec
        .clone();
    if !plan.ready_steps().contains(&spec.id.as_str()) {
        return Err(PlanError::NotReady.into());
    }
    // 先完成参数与路径绑定，再保存 Executing；缺少授权参数不消耗尝试次数。
    let observer = match spec.capability.as_str() {
        OBSERVE => {
            if options.output_file.is_some() {
                return Err("观察步骤不接受 --output。".into());
            }
            Some(BoundFileObserver::bind(
                options
                    .observe_file
                    .as_deref()
                    .ok_or("观察步骤需要 --observe-file。")?,
            )?)
        }
        EXPORT => {
            if options.observe_file.is_none() || options.output_file.is_none() {
                return Err("导出步骤需要 --observe-file 和 --output。".into());
            }
            None
        }
        _ => return Err(PlanError::UnknownCapability.into()),
    };
    let begun = journal.begin_step(&plan.id, &spec.id, plan.revision, at_ms)?;
    let started = begun
        .step(&spec.id)
        .and_then(|step| step.attempts.last())
        .ok_or(PlanError::CorruptState)?
        .started_at_ms;
    let outcome = match observer {
        Some(observer) => observe(observer, &plan.binding, &spec, started).await?,
        None => export(kernel, registry, admin, &options, &plan.binding, started).await?,
    };
    let finished = journal.finish_step(&plan.id, &spec.id, begun.revision, outcome)?;
    Ok(
        json!({"command": "plan-step", "executed": true, "step": finished.step(&spec.id),
        "plan": plan_view(&finished)}),
    )
}

async fn observe(
    observer: BoundFileObserver,
    binding: &PlanBinding,
    spec: &StepSpec,
    started: u64,
) -> Result<StepOutcome, AppError> {
    let goal_id = binding.goal_id.clone();
    let revision = binding.goal_revision;
    let read = tokio::task::spawn_blocking(move || {
        let at = now_ms().unwrap_or(started).max(started);
        observer.read_input(&goal_id, revision, at)
    });
    let at = |value: u64| value.max(started);
    // 只读文件：期限到达后不再等待结果，迟到的读取不影响任何状态。
    Ok(
        match tokio::time::timeout(Duration::from_millis(spec.timeout_ms), read).await {
            Ok(Ok(Ok(input))) => StepOutcome::Evidence(StepEvidence {
                capability: OBSERVE.into(),
                source_id: input.observation_source_id,
                sha256: input.sha256,
                bytes: input.byte_count,
                verified_at_ms: at(input.observed_at_ms),
            }),
            Ok(_) => StepOutcome::Failed {
                failure: StepFailure::CapabilityFailed,
                at_ms: at(now_ms()?),
            },
            Err(_) => StepOutcome::Failed {
                failure: StepFailure::Timeout,
                at_ms: at(now_ms()?),
            },
        },
    )
}

async fn export(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
    admin: &CognitionController,
    options: &PlanStepOptions,
    binding: &PlanBinding,
    started: u64,
) -> Result<StepOutcome, AppError> {
    let input_sha256 = binding
        .input_sha256
        .clone()
        .ok_or(PlanError::InvalidInput)?;
    let result = cognition_action::export_plan_record(
        kernel,
        registry,
        admin,
        ExportPlanOptions {
            goal_id: binding.goal_id.clone(),
            expected_goal_revision: binding.goal_revision,
            user_id: options.user_id.clone(),
            input_sha256,
            observe_file: options
                .observe_file
                .clone()
                .ok_or(PlanError::InvalidInput)?,
            output_file: options.output_file.clone().ok_or(PlanError::InvalidInput)?,
        },
    )
    .await;
    let at = now_ms()?.max(started);
    let report = match result {
        Ok(report) => report,
        // 准入失败时没有开始写入；按当前状态区分输入变化与能力失败。
        Err(_) => {
            let failure = match current_plan_binding(admin, &binding.goal_id, &options.user_id, at)
            {
                Ok((revision, input))
                    if revision == binding.goal_revision && input == binding.input_sha256 =>
                {
                    StepFailure::CapabilityFailed
                }
                _ => StepFailure::BindingChanged,
            };
            return Ok(StepOutcome::Failed { failure, at_ms: at });
        }
    };
    let record = report.record;
    Ok(match (record.status, record.receipt, record.failure) {
        (ActionStatus::Completed, Some(receipt), _) => StepOutcome::Evidence(StepEvidence {
            capability: EXPORT.into(),
            source_id: receipt.artifact_source_id,
            sha256: receipt.sha256,
            bytes: receipt.byte_count,
            verified_at_ms: receipt.verified_at_ms.max(started),
        }),
        (_, _, failure) => StepOutcome::Failed {
            failure: match failure {
                Some(ActionFailure::Interrupted) => StepFailure::Interrupted,
                Some(ActionFailure::Cancelled) => StepFailure::Cancelled,
                Some(ActionFailure::DeadlineExceeded) => StepFailure::Timeout,
                Some(ActionFailure::PreconditionChanged) => StepFailure::BindingChanged,
                _ => StepFailure::CapabilityFailed,
            },
            at_ms: at,
        },
    })
}
