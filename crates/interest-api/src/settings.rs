//! 兴趣观察的持久配置与运行时订阅；不依赖 HTTP、Kernel 或具体异步运行器。
use crate::{InterestFuture, InterestResult, ObservationOptions};
use serde::{Deserialize, Serialize};

pub const INTEREST_SETTINGS_PLUGIN_ID: &str = "eve.interest.settings";
pub const INTEREST_SETTINGS_STATE_KEY: &str = "settings.v1";

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterestLearningSettings {
    pub enabled: bool,
    pub observation: ObservationOptions,
}
impl InterestLearningSettings {
    pub fn validate(&self) -> InterestResult<()> {
        self.observation.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterestSettingsSnapshot {
    pub revision: u64,
    pub settings: InterestLearningSettings,
    /// 最近一次暂停的修订。快速暂停再启用仍须终结先前已经发出的请求。
    pub paused_at_revision: u64,
}

/// 宿主只获得读取和订阅能力；配置写入另由操作者授信。
pub trait InterestSettingsReader: Send + Sync {
    fn snapshot(&self) -> InterestResult<InterestSettingsSnapshot>;
    /// 等到 revision 变化或实例关闭；先订阅再核对，不能遗漏并发保存。
    fn changed(&self, revision: u64) -> InterestFuture<'_, InterestSettingsSnapshot>;
}
