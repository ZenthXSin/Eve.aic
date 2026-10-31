//! 普通闲聊邀请的候选规则；只读真实私聊证据，准入、预算和发送复用主动交流账本。
use crate::{AppError, qq_outreach::Services};
use eve_config_api::{ConfigField, ConfigKind, ConfigSchema, ConfigService};
use eve_memory_api::{EvidenceSource, MemoryAdmin, MemoryScope};
use eve_outreach_api::{
    Fact, FactKind, IdleChatSource, Invitation, InvitationOrigin, OutreachSnapshot,
};
use eve_qqbot_plugin::QqInteractionSource;
use std::{collections::BTreeMap, time::Duration};
pub(crate) const CHAT_IDLE_MS: u64 = 6 * 60 * 60 * 1000;
pub(crate) const CHAT_MAX_GAP_MS: u64 = 30 * 24 * 60 * 60 * 1000;
pub(crate) const MAX_CHAT_SCOPES: usize = 64;
pub(crate) const CHAT_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const NAMESPACE: &str = "runtime.idle_chat";
pub(crate) fn schema() -> ConfigSchema {
    let mut field = ConfigField::new(
        ConfigKind::Integer {
            minimum: Some(0),
            maximum: Some(CHAT_MAX_GAP_MS as i64),
        },
        Some(serde_json::json!(CHAT_IDLE_MS)),
    );
    field.environment = Some("EVE_IDLE_CHAT_AFTER_MS".into());
    ConfigSchema {
        namespace: NAMESPACE.into(),
        version: 1,
        fields: BTreeMap::from([("after_ms".into(), field)]),
    }
}
pub(crate) fn after(settings: &dyn ConfigService) -> Result<u64, AppError> {
    Ok(settings.snapshot(NAMESPACE, 1)?.get("after_ms")?)
}
pub(crate) struct Candidate {
    pub owner: String,
    pub source: IdleChatSource,
    pub facts: Vec<Fact>,
}
fn last(
    memory: &dyn MemoryAdmin,
    session: &str,
    user: &str,
) -> Result<Option<(String, u64)>, AppError> {
    let snapshot = memory
        .reader(MemoryScope {
            channel: "qq".into(),
            session_id: session.into(),
            user_id: user.into(),
        })?
        .snapshot()?;
    Ok(snapshot
        .evidence
        .iter()
        .filter(|e| matches!(&e.source, EvidenceSource::CompletedInteraction { .. }))
        .max_by_key(|e| e.at_ms)
        .map(|e| (e.id.clone(), e.at_ms)))
}
fn eligible(
    snapshot: &OutreachSnapshot,
    owner: &str,
    source: &IdleChatSource,
    now: u64,
    after: u64,
) -> bool {
    let gap = now.saturating_sub(source.last_active_at_ms);
    after > 0
        && source.last_active_at_ms > 0
        && now > source.last_active_at_ms
        && gap >= after
        && gap < CHAT_MAX_GAP_MS
        && !snapshot.quiet(owner)
        && snapshot.negative_streak(owner) == 0
        && !snapshot
            .for_owner(owner)
            .any(|i| i.status.is_open() || i.created_at_ms >= source.last_active_at_ms)
        && snapshot.for_goal(&source.key(owner)).is_none()
}
pub(crate) fn candidate(services: &Services, now: u64) -> Result<Option<Candidate>, AppError> {
    let after = after(services.settings.as_ref())?;
    if after == 0 {
        return Ok(None);
    }
    let snapshot = services.outreach.snapshot()?;
    for identity in services
        .conversations
        .recent_identities()?
        .into_iter()
        .filter(|i| i.source == QqInteractionSource::DirectMessage)
        .take(MAX_CHAT_SCOPES)
    {
        let session = identity.session;
        let Some((evidence_id, last_active_at_ms)) = last(
            services.memory.as_ref(),
            &session.session_id,
            &session.user_id,
        )?
        else {
            continue;
        };
        let source = IdleChatSource {
            session_id: session.session_id,
            evidence_id,
            last_active_at_ms,
        };
        if eligible(&snapshot, &session.user_id, &source, now, after) {
            return Ok(Some(Candidate {
                owner: session.user_id,
                source,
                facts: vec![Fact {
                    kind: FactKind::IdleContext,
                    text: format!(
                        "距这位用户上次完成的真实私聊已 {} 小时。除此之外没有可核对的近况，不得推断情绪或忙闲。",
                        (now - last_active_at_ms) / 3_600_000
                    ),
                }],
            }));
        }
    }
    Ok(None)
}
/// 用户已回来、热关闭或超过最大间隔时丢弃未发送旧邀请；不把闲聊交给认知目标取消规则。
pub(crate) fn still_current(
    invitation: &Invitation,
    memory: &dyn MemoryAdmin,
    settings: &dyn ConfigService,
    now: u64,
) -> Result<bool, AppError> {
    let InvitationOrigin::IdleChat(source) = &invitation.origin else {
        return Ok(true);
    };
    let threshold = after(settings)?;
    let gap = now.saturating_sub(source.last_active_at_ms);
    Ok(threshold > 0
        && gap >= threshold
        && gap < CHAT_MAX_GAP_MS
        && last(memory, &source.session_id, &invitation.owner)?
            .is_some_and(|(id, at)| id == source.evidence_id && at == source.last_active_at_ms))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn six_hour_to_thirty_day_window_and_quiet_are_enforced() {
        let mut s = OutreachSnapshot {
            invitations: vec![],
            preferences: vec![],
        };
        let source = IdleChatSource {
            session_id: "s".into(),
            evidence_id: "m".into(),
            last_active_at_ms: 1,
        };
        assert!(!eligible(&s, "u", &source, CHAT_IDLE_MS, CHAT_IDLE_MS));
        assert!(eligible(&s, "u", &source, 1 + CHAT_IDLE_MS, CHAT_IDLE_MS));
        assert!(!eligible(
            &s,
            "u",
            &source,
            1 + CHAT_MAX_GAP_MS,
            CHAT_IDLE_MS
        ));
        assert!(!eligible(&s, "u", &source, 1 + CHAT_IDLE_MS, 0));
        s.preferences.push(eve_outreach_api::OwnerPreference {
            owner: "u".into(),
            quiet: true,
            changed_at_ms: 1,
        });
        assert!(!eligible(&s, "u", &source, 1 + CHAT_IDLE_MS, CHAT_IDLE_MS));
    }
}
