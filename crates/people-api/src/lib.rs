//! 人名称呼目录：标签与宿主身份分离，不提供任何授权能力。
use eve_plugin_api::{PluginFuture, PluginResult};
use serde::{Deserialize, Serialize};

pub const PEOPLE_PLUGIN_ID: &str = "eve.people";
pub const MAX_PEOPLE: usize = 1024;
pub const MAX_VISIBLE_PEOPLE: usize = 16;
pub const MAX_STATE_BYTES: usize = 1024 * 1024;

pub fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.len() <= 128
        && value.chars().count() <= 48
        && !value.chars().any(char::is_control)
}
pub fn valid_identity(value: &str) -> bool {
    value
        .strip_prefix("qq:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}
#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameSource {
    Platform,
    Declared,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonScope {
    pub conversation: String,
    pub participant: String,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonName {
    pub value: String,
    pub source: NameSource,
}
/// 每人至多保存的印象条数。
pub const MAX_IMPRESSIONS: usize = 12;
/// 每人至多缓冲的待学原话；攒够 `IMPRESSION_BATCH` 条成批。
pub const MAX_IMPRESSION_PENDING: usize = 8;
pub const IMPRESSION_BATCH: usize = 4;
/// 学习器单批至多返回的印象条数。
pub const MAX_IMPRESSION_DRAFTS: usize = 3;
/// 印象正文与原话摘录的字符上限。
pub const MAX_IMPRESSION_CHARS: usize = 60;

/// 这个人自己说过的事；`quote` 是其原话中的原样摘录，用作来源。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Impression {
    pub id: String,
    pub text: String,
    pub quote: String,
    pub updated_at_ms: u64,
}

/// 学习器候选；`replaces` 指向因本批原话而过时的旧印象。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpressionDraft {
    pub text: String,
    pub quote: String,
    #[serde(default)]
    pub replaces: Option<String>,
}

/// 一批待学习的原话及此人现有印象。取出即从缓冲移除，失败或中断不重试。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImpressionBatch {
    pub id: String,
    pub participant: String,
    pub messages: Vec<String>,
    pub known: Vec<Impression>,
}

/// 可替换的印象学习器；只看本批原话与现有印象，不调用工具。
pub trait ImpressionLearner: Send + Sync {
    fn learn(&self, batch: ImpressionBatch) -> PluginFuture<'_, Vec<ImpressionDraft>>;
}

/// 跨会话合并时至多带上的他处印象条数。
pub const MAX_LINKED_IMPRESSIONS: usize = 8;

/// 同一个人在其他会话里说过的事；`scene` 是来源会话的场景，旧记录没有来访信息时为空。
/// 只读，删除须回到来源会话。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LinkedImpression {
    pub text: String,
    pub quote: String,
    pub updated_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scene: Option<Scene>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LinkedImpressions {
    pub items: Vec<LinkedImpression>,
    /// 经授权的来源参与者数（其印象可能都与本会话重复）。
    pub sources: usize,
    pub truncated: bool,
}

/// 由可信宿主决定其他参与者中哪些与当前发言者是同一个人、且在当前会话可见。
/// 只能依据已校验的平台身份与明确绑定，不依据昵称、自称或对话文字；返回值必须是 `candidates` 的子集。
pub trait PersonLinks: Send + Sync {
    fn linked(&self, current: &str, candidates: &[String]) -> PluginResult<Vec<String>>;
}

pub fn valid_impression_text(value: &str) -> bool {
    let count = value.chars().count();
    (1..=MAX_IMPRESSION_CHARS).contains(&count)
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

/// 来访场景；同一参与者的身份摘要已包含私聊/群，场景不会变。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scene {
    Private,
    Group,
}
/// 宿主按每条收到的消息记录的来访情况；只来自平台事件，不来自对话文字。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Presence {
    pub scene: Scene,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// 本条之前最近一次来访；第一次来时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_seen_ms: Option<u64>,
    pub messages: u64,
}
impl Presence {
    pub fn valid(&self) -> bool {
        self.messages > 0
            && self.first_seen_ms <= self.last_seen_ms
            && self.previous_seen_ms.is_none_or(|p| {
                self.messages > 1 && self.first_seen_ms <= p && p <= self.last_seen_ms
            })
            && (self.messages == 1) == self.previous_seen_ms.is_none()
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub scope: PersonScope,
    pub name: Option<PersonName>,
    pub revision: u64,
    /// 只来自此人自己的话；视图只给当前发言者本人的印象。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impressions: Vec<Impression>,
    /// 待学原话，只在持久状态中出现，任何视图都不包含。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    /// 来访情况；视图只给当前发言者本人。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<Presence>,
}
#[derive(Clone, Serialize)]
pub struct PeopleView {
    pub current: Person,
    pub others: Vec<Person>,
    pub truncated: bool,
}
/// 仅可信宿主调用写入；模型只收到当前会话的有界只读视图。
pub trait PeopleDirectory: Send + Sync {
    fn observe(&self, scope: PersonScope, name: Option<PersonName>) -> PluginResult<()>;
    /// 当前发言者本人带印象，其他人只有称呼；待学原话不出现在视图中。
    fn view(&self, participant: &str) -> PluginResult<Option<PeopleView>>;
    /// 缓冲此人一条可能谈及自己的原话；未开启印象学习时忽略。须先 observe 过此人。
    fn note(&self, _participant: &str, _text: &str) -> PluginResult<()> {
        Ok(())
    }
    /// 记一次来访：须先 observe 过此人。时间倒退时不改写已有记录。不实现时不记录。
    fn visit(&self, _participant: &str, _scene: Scene, _now_ms: u64) -> PluginResult<()> {
        Ok(())
    }
    fn take_impression_batch(&self) -> PluginResult<Option<ImpressionBatch>> {
        Ok(None)
    }
    /// 核对并保存，返回新增或替换的条数；任一候选不合格时整批拒绝。
    fn finish_impression_batch(
        &self,
        _batch: &ImpressionBatch,
        _drafts: Vec<ImpressionDraft>,
        _now_ms: u64,
    ) -> PluginResult<usize> {
        Ok(0)
    }
    fn impressions(&self, _participant: &str) -> PluginResult<Vec<Impression>> {
        Ok(vec![])
    }
    fn forget_impression(&self, _participant: &str, _id: &str) -> PluginResult<bool> {
        Ok(false)
    }
    /// 只读：当前发言者在其他会话中、经 `links` 授权的印象。去掉与本会话印象相同的，
    /// 按更新时间从新到旧至多 `MAX_LINKED_IMPRESSIONS` 条。不实现时为空。
    fn linked_impressions(
        &self,
        _participant: &str,
        _links: &dyn PersonLinks,
    ) -> PluginResult<LinkedImpressions> {
        Ok(LinkedImpressions::default())
    }
}
