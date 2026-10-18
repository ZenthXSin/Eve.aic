//! 人名称呼目录：标签与宿主身份分离，不提供任何授权能力。
use eve_plugin_api::PluginResult;
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
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Person {
    pub scope: PersonScope,
    pub name: Option<PersonName>,
    pub revision: u64,
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
    fn view(&self, participant: &str) -> PluginResult<Option<PeopleView>>;
}
