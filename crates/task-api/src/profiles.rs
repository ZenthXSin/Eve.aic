//! 子代理配置定义；角色说明不提供额外工具权限或独立预算。
use eve_llm_api::ContextScope;
use serde::{Deserialize, Serialize};
use std::fmt;

pub const SUBAGENT_FORBIDDEN_TOOLS: &[&str] = &[
    "dispatch_background_task",
    "list_background_tasks",
    "cancel_background_task",
    "run_subagent",
    "set_wakeup_trigger",
    "list_wakeup_triggers",
    "cancel_wakeup_trigger",
    "create_subagent",
    "list_subagents",
    "delete_subagent",
];
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentProfileDraft {
    pub name: String,
    pub instructions: String,
    /// None 继承叶子宿主已有工具；Some 只取指定工具与宿主的交集，空列表为纯模型。
    pub tools: Option<Vec<String>>,
}
impl SubagentProfileDraft {
    pub fn validate(&self) -> Result<(), SubagentProfileError> {
        if self.name.trim() != self.name
            || self.name.is_empty()
            || self.name.len() > 128
            || self.name.chars().any(char::is_control)
            || self.instructions.trim().is_empty()
            || self.instructions.len() > 8192
            || self.instructions.contains('\0')
            || self.tools.as_ref().is_some_and(|tools| {
                tools.len() > 128
                    || tools.iter().enumerate().any(|(i, n)| {
                        n.is_empty()
                            || n.len() > 128
                            || n.trim() != n
                            || n.chars().any(char::is_control)
                            || SUBAGENT_FORBIDDEN_TOOLS.contains(&n.as_str())
                            || tools[..i].contains(n)
                    })
            })
        {
            return Err(SubagentProfileError::InvalidInput);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentProfile {
    pub id: String,
    pub revision: u64,
    pub profile: SubagentProfileDraft,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentProfileError {
    InvalidInput,
    NotFound,
    Conflict,
    Storage,
    CorruptState,
}
impl fmt::Display for SubagentProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "子代理配置输入无效或包含编排工具",
            Self::NotFound => "当前范围没有这个子代理配置",
            Self::Conflict => "同名配置不同或配置版本已改变",
            Self::Storage => "子代理配置存储不可用；原记录保留",
            Self::CorruptState => "子代理配置数据库损坏或版本不兼容；未清空",
        })
    }
}
impl std::error::Error for SubagentProfileError {}
pub trait SubagentProfiles: Send + Sync {
    fn create(
        &self,
        scope: &ContextScope,
        draft: SubagentProfileDraft,
    ) -> Result<SubagentProfile, SubagentProfileError>;
    fn get(&self, scope: &ContextScope, id: &str) -> Result<SubagentProfile, SubagentProfileError>;
    /// 每页最多 64 条，after 为上页最后一条 ID。
    fn list(
        &self,
        scope: &ContextScope,
        after: Option<&str>,
    ) -> Result<Vec<SubagentProfile>, SubagentProfileError>;
    fn delete(
        &self,
        scope: &ContextScope,
        id: &str,
        revision: u64,
    ) -> Result<(), SubagentProfileError>;
}
