//! 本地操作者显式授权的无模型草稿导出组合；文件路径不进入公开提案或状态。
use crate::{
    AppError,
    cognition_action_admission::{
        DocumentActionRequest, prepare_proposal, revalidate_proposal, validate_access,
    },
};
use eve_action_api::{
    ACTION_PLUGIN_ID, ActionError, ActionJournal, ActionPrecondition, ActionRecord, ActionResult,
    DocumentActionProposal,
};
use eve_action_plugin::{
    ActionCancellation, ActionController, ActionPlugin, execute_document_action,
};
use eve_artifact_file::BoundArtifactFile;
use eve_cognition_plugin::CognitionController;
use eve_file_observer::BoundFileObserver;
use eve_kernel::Kernel;
use eve_plugin_api::{PluginId, ServiceId, ServiceRegistry};
use eve_session_api::{SESSION_PLUGIN_ID, SESSION_SERVICE_ID, SessionServiceHandle};
use eve_session_plugin::SessionPlugin;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub(crate) struct ExportPlanOptions {
    pub goal_id: String,
    pub expected_goal_revision: u64,
    pub user_id: String,
    pub input_sha256: String,
    pub observe_file: PathBuf,
    pub output_file: PathBuf,
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn now_ms() -> Result<u64, AppError> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

async fn open_actions(kernel: &Kernel) -> Result<ActionController, AppError> {
    let plugin = ActionPlugin::new("eve")?;
    let controller = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start(&PluginId::new(ACTION_PLUGIN_ID)?).await?;
    Ok(controller)
}

pub(crate) async fn action_records(
    kernel: &Kernel,
    goal_id: &str,
) -> Result<Vec<ActionRecord>, AppError> {
    let controller = open_actions(kernel).await?;
    Ok(controller
        .snapshot()?
        .records
        .into_iter()
        .filter(|record| {
            record.proposal.goal_id == goal_id || record.proposal.reflection_goal_id == goal_id
        })
        .collect())
}

struct BoundPrecondition {
    admin: CognitionController,
    observer: BoundFileObserver,
}
impl ActionPrecondition for BoundPrecondition {
    fn check(&self, proposal: &DocumentActionProposal) -> ActionResult<()> {
        let time = now_ms().map_err(|_| ActionError::Unavailable)?;
        let observed = self
            .observer
            .read_input(&proposal.goal_id, proposal.goal_revision, time)
            .map_err(|_| ActionError::InvalidTransition)?;
        revalidate_proposal(&self.admin, proposal, &observed, time).map_err(|error| {
            error
                .downcast_ref::<ActionError>()
                .copied()
                .unwrap_or(ActionError::InvalidTransition)
        })
    }
}

pub(crate) async fn export_plan(
    kernel: &Kernel,
    registry: &Arc<dyn ServiceRegistry>,
    admin: &CognitionController,
    options: ExportPlanOptions,
) -> Result<Value, AppError> {
    let request = DocumentActionRequest {
        goal_id: options.goal_id,
        expected_goal_revision: options.expected_goal_revision,
        user_id: options.user_id,
        input_sha256: options.input_sha256,
    };
    validate_access(admin, &request, now_ms()?)?;
    // 先恢复行动日志；上次未结束的写入只会封存 Interrupted，不会获得重放机会。
    let journal = open_actions(kernel).await?;
    kernel.register(Box::new(SessionPlugin::new()?))?;
    kernel.start(&PluginId::new(SESSION_PLUGIN_ID)?).await?;
    let sessions = registry
        .get(&ServiceId::new(SESSION_SERVICE_ID)?)?
        .ok_or("会话服务缺失。")?
        .value
        .downcast::<SessionServiceHandle>()
        .map_err(|_| "会话服务类型错误。")?;
    let observer = BoundFileObserver::bind(&options.observe_file)?;
    let observed =
        observer.read_input(&request.goal_id, request.expected_goal_revision, now_ms()?)?;
    let target = BoundArtifactFile::bind(&options.output_file)?;
    let prepared = prepare_proposal(
        admin,
        sessions.0.as_ref(),
        &request,
        &observed,
        target.source_id(),
        now_ms()?,
    )?;
    let precondition = BoundPrecondition {
        admin: admin.clone(),
        observer,
    };
    let cancellation = ActionCancellation::default();
    let execution = execute_document_action(
        Arc::new(journal),
        Arc::new(target),
        prepared.proposal,
        prepared.bytes,
        Arc::new(precondition),
        cancellation.clone(),
    );
    tokio::pin!(execution);
    // 中断只请求取消；必须等待这个行动自己的 worker 收尾并保存，不丢弃写入任务。
    let result = tokio::select! {
        result = &mut execution => result?,
        signal = interrupted() => {
            cancellation.cancel();
            let result = execution.await;
            signal?;
            result?
        }
    };
    Ok(json!({"command": "export-plan", "action": result}))
}

async fn interrupted() -> Result<(), AppError> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
