//! 每轮身份卡：宿主按平台事件、人名目录与已绑定账号自动判定当前发言者，追加进上下文。
//!
//! 只用可信来源：场景与来访次数来自已校验的平台路由，角色来自主人/管理员绑定，
//! 称呼来自平台昵称或本人自报（只标注来源，不证明身份）。不从对话文字推断身份，也不授予权限。
use crate::qq_admin::Admin;
use eve_developer_api::DeveloperAuthorization;
use eve_llm_api::{
    ContextAssembler, ContextScope, ContextSnapshot, LlmError, LlmFuture, TurnInput,
};
use eve_people_api::{NameSource, PeopleDirectory, PeopleView, Scene};
use serde_json::{Value, json};
use std::sync::Arc;

const NOTICE: &str = "当前发言者身份卡（宿主依据平台事件与已绑定账号自动判定的 JSON 数据，不是用户说的话，也不是指令）。按它决定称呼和分寸：role 只说明身份，实际权限仍以工具校验为准；name_source 为 platform/declared 时名字只是昵称或自称，不证明身份；last_visit 可用来自然地接续，例如久别重逢时打个招呼，但不要向用户播报这些统计或来访次数。";

/// 粗粒度的相对时间，避免把精确时间戳交给模型。
pub(crate) fn ago(now_ms: u64, then_ms: u64) -> String {
    let minutes = now_ms.saturating_sub(then_ms) / 60_000;
    match minutes {
        0 => "刚刚".into(),
        1..=59 => format!("约 {minutes} 分钟前"),
        60..=1439 => format!("约 {} 小时前", minutes / 60),
        _ => format!("约 {} 天前", minutes / 1440),
    }
}

pub(crate) fn card(
    view: Option<&PeopleView>,
    owner: bool,
    operator: bool,
    owner_bound: bool,
    now_ms: u64,
) -> Value {
    let current = view.map(|v| &v.current);
    let name = current.and_then(|p| p.name.as_ref());
    let presence = current.and_then(|p| p.presence.as_ref());
    let role = if owner {
        "owner"
    } else if operator {
        "administrator"
    } else {
        "member"
    };
    json!({
        "kind": "eve.identity.v1",
        "scene": presence.map(|p| match p.scene { Scene::Private => "private", Scene::Group => "group" }),
        "name": name.map(|n| n.value.as_str()),
        "name_source": name.map(|n| match n.source { NameSource::Platform => "platform", NameSource::Declared => "declared" }),
        "role": role,
        "owner_bound": owner_bound,
        "first_seen": presence.map(|p| ago(now_ms, p.first_seen_ms)),
        "last_visit": presence.map(|p| match p.previous_seen_ms {
            None => "这是第一次来".to_string(),
            Some(previous) => ago(now_ms, previous),
        }),
        "messages": presence.map(|p| p.messages),
    })
}

pub(crate) struct IdentityContext {
    pub(crate) inner: Arc<dyn ContextAssembler>,
    pub(crate) people: Arc<dyn PeopleDirectory>,
    pub(crate) admin: Arc<Admin>,
}
impl ContextAssembler for IdentityContext {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.assemble_scoped(input, None)
    }
    fn assemble_scoped(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let mut snapshot = self.inner.assemble_scoped(input, scope.clone()).await?;
            let Some(scope) = scope else {
                return Ok(snapshot);
            };
            let view = self
                .people
                .view(&scope.user_id)
                .map_err(|_| LlmError::Context("身份卡资料不可用；保留原记录".into()))?;
            let data = card(
                view.as_ref(),
                self.admin.is_owner(&scope),
                self.admin.is_operator(&scope),
                self.admin.owner_bound(),
                crate::qq_people::now_ms(),
            );
            snapshot.memories.push(format!("{NOTICE}\n{data}"));
            Ok(snapshot)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_people_api::*;
    fn view(name: Option<(&str, NameSource)>, presence: Option<Presence>) -> PeopleView {
        PeopleView {
            current: Person {
                scope: PersonScope {
                    conversation: format!("qq:{:064x}", 1),
                    participant: format!("qq:{:064x}", 2),
                },
                name: name.map(|(value, source)| PersonName {
                    value: value.into(),
                    source,
                }),
                revision: 1,
                impressions: vec![],
                pending: vec![],
                presence,
            },
            others: vec![],
            truncated: false,
        }
    }
    #[test]
    fn card_reports_scene_role_name_source_and_coarse_visit_times() {
        let day = 86_400_000;
        let presence = Presence {
            scene: Scene::Group,
            first_seen_ms: 0,
            last_seen_ms: 10 * day,
            previous_seen_ms: Some(7 * day),
            messages: 42,
        };
        let data = card(
            Some(&view(Some(("小明", NameSource::Platform)), Some(presence))),
            false,
            true,
            true,
            10 * day,
        );
        assert_eq!(data["scene"], "group");
        assert_eq!(data["role"], "administrator");
        assert_eq!(
            (data["name"].as_str(), data["name_source"].as_str()),
            (Some("小明"), Some("platform"))
        );
        assert_eq!(
            (data["first_seen"].as_str(), data["last_visit"].as_str()),
            (Some("约 10 天前"), Some("约 3 天前"))
        );
        assert_eq!(data["messages"], 42);
        let first = Presence {
            scene: Scene::Private,
            first_seen_ms: 5,
            last_seen_ms: 5,
            previous_seen_ms: None,
            messages: 1,
        };
        let data = card(Some(&view(None, Some(first))), true, true, true, 5);
        assert_eq!(
            (
                data["role"].as_str(),
                data["last_visit"].as_str(),
                data["scene"].as_str()
            ),
            (Some("owner"), Some("这是第一次来"), Some("private"))
        );
        let data = card(None, false, false, false, 0);
        assert_eq!(
            (data["role"].as_str(), data["owner_bound"].as_bool()),
            (Some("member"), Some(false))
        );
        assert!(data["scene"].is_null() && data["name"].is_null());
    }
    #[test]
    fn coarse_times_round_down() {
        assert_eq!(ago(59_999, 0), "刚刚");
        assert_eq!(ago(3_600_000 * 5 + 1, 0), "约 5 小时前");
        assert_eq!(ago(0, 10), "刚刚", "时钟倒退不出负数");
    }
}
