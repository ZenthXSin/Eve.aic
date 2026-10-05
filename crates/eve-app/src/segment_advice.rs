//! QQ 宿主把当前范围已确认 Memory 偏好交给公开建议器；明确采用后只写一次分段设置。
use crate::segment_commands::{SegmentCommand, status};
use eve_memory_api::{MemoryAdmin, PreferenceStatus};
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandInput, segment_scope};
use eve_segment_api::{
    ConfirmedSegmentFeedback, SegmentAdviceRequest, SegmentAdvisor, SegmentChange, SegmentPolicy,
    SegmentPreferenceError, SegmentPreferences, segment_advice,
};
use std::{collections::BTreeSet, sync::Arc};

const PAGE_SIZE: usize = 5;
pub(crate) struct Commands {
    memory: Arc<dyn MemoryAdmin>,
    advisor: Arc<dyn SegmentAdvisor>,
    automatic: bool,
}
fn failure() -> PluginError {
    PluginError::State(
        "节奏建议来源或设置提交无法确认；服务已停止，请重新打开后查看持久状态。".into(),
    )
}
impl Commands {
    pub(crate) fn new(
        memory: Arc<dyn MemoryAdmin>,
        advisor: Arc<dyn SegmentAdvisor>,
        automatic: bool,
    ) -> Self {
        Self {
            memory,
            advisor,
            automatic,
        }
    }
    pub(crate) fn execute(
        &self,
        input: &QqCommandInput<'_>,
        store: &dyn SegmentPreferences,
        policy: &SegmentPolicy,
        command: SegmentCommand<'_>,
    ) -> PluginResult<String> {
        let scope = crate::qq_memory::scope(input.session);
        let (snapshot, feedback) = feedback(self.memory.as_ref(), &scope)?;
        let active: Vec<_> = snapshot
            .preferences
            .iter()
            .filter(|p| p.status == PreferenceStatus::Confirmed)
            .collect();
        if let SegmentCommand::Adopt(id, revision) = command {
            let Some(preference) = active.iter().find(|p| p.id == id) else {
                return Ok("当前会话没有这条已确认偏好；已撤销的偏好不能采用。".into());
            };
            if preference.revision != revision {
                return Ok("偏好版本已变化；请重新发送 /segment suggestions 查看后再采用。".into());
            }
        }
        let segment_scope = segment_scope(input.session);
        let advice = segment_advice(
            self.advisor.as_ref(),
            &SegmentAdviceRequest {
                scope: &segment_scope,
                feedback: &feedback,
                policy,
            },
        )
        .map_err(|_| failure())?;
        match command {
            SegmentCommand::Suggestions(page) => {
                if advice.suggestions.is_empty() {
                    return Ok("当前会话没有可采用的节奏建议。可先确认“回复最多分成两段，段间不要停顿”等明确偏好，再查看 /segment suggestions。".into());
                }
                let pages = advice.suggestions.len().div_ceil(PAGE_SIZE);
                if page > pages {
                    return Ok(format!(
                        "节奏建议共有 {pages} 页；发送 /segment suggestions 1 查看第一页。"
                    ));
                }
                let mut reply =
                    format!("当前会话节奏建议（第 {page}/{pages} 页；查看不改变设置）：");
                for suggestion in advice
                    .suggestions
                    .iter()
                    .skip((page - 1) * PAGE_SIZE)
                    .take(PAGE_SIZE)
                {
                    let source = feedback
                        .iter()
                        .find(|f| f.source_id == suggestion.source_id)
                        .ok_or_else(failure)?;
                    let mut changes = Vec::new();
                    if let Some(enabled) = suggestion.patch.enabled {
                        changes.push(
                            if enabled {
                                "开启分段"
                            } else {
                                "整条发送"
                            }
                            .into(),
                        );
                    }
                    if let Some(n) = suggestion.patch.max_segments {
                        changes.push(format!("最多 {n} 段"));
                    }
                    if let Some(p) = suggestion.patch.pause_percent {
                        changes.push(format!("段间停顿 {p}%"));
                    }
                    // 正文预览仍属当前用户已确认的偏好；命令只带来源标识和确切版本。
                    let preview: String = source
                        .text
                        .chars()
                        .take(96)
                        .map(|c| if c.is_control() { ' ' } else { c })
                        .collect();
                    reply.push_str(&format!(
                        "\n{} 第 {} 版：{}。来源：{}\n采用：/segment adopt {} {}",
                        source.source_id,
                        source.source_revision,
                        changes.join("、"),
                        preview,
                        source.source_id,
                        source.source_revision
                    ));
                }
                reply.push_str(if self.automatic {
                    "\n自主学习已开启：有效节奏偏好自动参与后续发送，手动设置优先。adopt 保存为手动设置；reset 清除手动设置并跟随学习；forget 撤销来源后不再自动使用，已手动采用的设置仍须 reset。"
                } else {
                    "\n只有明确采用才修改设置；未涉及的项保持当前值。/segment reset 恢复默认；/forget 偏好ID 撤销来源，已应用的设置需另行恢复。"
                });
                Ok(reply)
            }
            SegmentCommand::Adopt(id, revision) => {
                let Some(suggestion) = advice
                    .suggestions
                    .iter()
                    .find(|s| s.source_id == id && s.source_revision == revision)
                else {
                    return Ok("这条偏好没有可采用的明确节奏建议；当前设置未改变。".into());
                };
                let next =
                    match store.update(&segment_scope, SegmentChange::Patch(suggestion.patch)) {
                        Ok(next) => next,
                        Err(SegmentPreferenceError::LimitReached) => {
                            return Ok("分段设置容量已满；原设置保留，本次没有保存。".into());
                        }
                        Err(_) => return Err(failure()),
                    };
                if next.validate().is_err()
                    || next
                        .apply(SegmentChange::Patch(suggestion.patch))
                        .map_err(|_| failure())?
                        != next
                {
                    return Err(failure());
                }
                Ok(format!(
                    "已采用偏好 {id} 第 {revision} 版的节奏建议。{} 可用 /segment reset 恢复默认；撤销来源不会自动回滚已应用的设置。",
                    status(&next, policy)
                ))
            }
            _ => Err(failure()),
        }
    }
}

/// 返回来源修订从新到旧的已确认反馈，读取不改变 Memory。
fn feedback(
    memory: &dyn MemoryAdmin,
    scope: &eve_memory_api::MemoryScope,
) -> PluginResult<(
    eve_memory_api::MemorySnapshot,
    Vec<ConfirmedSegmentFeedback>,
)> {
    let snapshot = memory
        .reader(scope.clone())
        .and_then(|reader| reader.snapshot())
        .map_err(|_| failure())?;
    if snapshot.scope != *scope {
        return Err(failure());
    }
    let mut ids = BTreeSet::new();
    let mut active = Vec::new();
    for preference in &snapshot.preferences {
        if !ids.insert(&preference.id) {
            return Err(failure());
        }
        if preference.status != PreferenceStatus::Confirmed {
            continue;
        }
        let latest = preference.history.last().ok_or_else(failure)?;
        if latest.revision != preference.revision
            || latest.text != preference.text
            || latest.status != preference.status
            || !snapshot.evidence.iter().any(|e| e.id == latest.evidence_id)
        {
            return Err(failure());
        }
        active.push(preference);
    }
    active.sort_by(|a, b| {
        let revision = |p: &eve_memory_api::Preference| {
            snapshot
                .evidence
                .iter()
                .find(|e| e.id == p.history.last().unwrap().evidence_id)
                .unwrap()
                .revision
        };
        revision(b).cmp(&revision(a)).then_with(|| a.id.cmp(&b.id))
    });
    let feedback = active
        .iter()
        .map(|p| ConfirmedSegmentFeedback {
            source_id: p.id.clone(),
            source_revision: p.revision,
            text: p.text.clone(),
        })
        .collect();
    Ok((snapshot, feedback))
}

/// 自主模式在发送时派生节奏；不复制偏好到第二份持久状态，不产生跨插件事务。
/// 最新来源逐字段覆盖旧来源；手动设置最后覆盖。撤销立即退出下一次派生。
pub(crate) struct AutomaticPreferences {
    pub(crate) manual: Arc<dyn SegmentPreferences>,
    pub(crate) memory: Arc<dyn MemoryAdmin>,
    pub(crate) advisor: Arc<dyn SegmentAdvisor>,
    pub(crate) policy: SegmentPolicy,
}
impl SegmentPreferences for AutomaticPreferences {
    fn get(
        &self,
        scope: &eve_segment_api::SegmentScope,
    ) -> Result<eve_segment_api::SegmentPreference, SegmentPreferenceError> {
        scope.validate()?;
        let (_, feedback) = feedback(
            self.memory.as_ref(),
            &eve_memory_api::MemoryScope {
                channel: scope.channel.clone(),
                session_id: scope.session_id.clone(),
                user_id: scope.user_id.clone(),
            },
        )
        .map_err(|_| SegmentPreferenceError::CorruptState)?;
        let advice = segment_advice(
            self.advisor.as_ref(),
            &SegmentAdviceRequest {
                scope,
                feedback: &feedback,
                policy: &self.policy,
            },
        )
        .map_err(|_| SegmentPreferenceError::CorruptState)?;
        let mut result = eve_segment_api::SegmentPreference::default();
        for source in feedback.iter().rev() {
            if let Some(s) = advice
                .suggestions
                .iter()
                .find(|s| s.source_id == source.source_id)
            {
                result = result.apply(SegmentChange::Patch(s.patch))?;
            }
        }
        result.apply(SegmentChange::Patch(self.manual.get(scope)?))
    }
    fn update(
        &self,
        scope: &eve_segment_api::SegmentScope,
        change: SegmentChange,
    ) -> Result<eve_segment_api::SegmentPreference, SegmentPreferenceError> {
        self.manual.update(scope, change)?;
        self.get(scope)
    }
}
