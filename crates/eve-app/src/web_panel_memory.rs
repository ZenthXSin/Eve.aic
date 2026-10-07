//! 记忆状态到面板只读契约的投影；面板只能枚举作用域并读取已有作用域，不写记忆。
use crate::web_panel::clip;
use eve_memory_api::{
    EvidenceSource, MemoryAdmin, MemoryError, MemoryScope, MemorySnapshot, Preference,
    PreferenceStatus, PreferenceVersion,
};
use eve_web_panel_api::*;
use std::sync::Arc;

/// 当前偏好正文受 MAX_PREFERENCE_BYTES 约束，详情完整显示；历史版本只给预览。
const PREFERENCE_TEXT_BYTES: usize = 4096;
const HISTORY_TEXT_BYTES: usize = 512;
const EVIDENCE_TEXT_BYTES: usize = 2048;
const REFERENCE_LIMIT: usize = 50;

/// 宿主持有的记忆管理能力只经此类型的读取方法进入面板；确认、更正和撤销不在其中。
pub(crate) struct MemoryView(Arc<dyn MemoryAdmin>);
impl MemoryView {
    pub(crate) fn new(admin: Arc<dyn MemoryAdmin>) -> Self {
        Self(admin)
    }
    fn scopes(&self) -> PanelResult<Vec<MemoryScope>> {
        let mut scopes = self.0.scopes().map_err(memory_error)?;
        scopes.sort();
        Ok(scopes)
    }
    /// 记忆实现对未知作用域返回空快照；面板先确认作用域已持久保存，避免把输错当成“没有记忆”。
    fn snapshot(&self, scope: &MemoryScope) -> PanelResult<MemorySnapshot> {
        if !self.scopes()?.contains(scope) {
            return Err(PanelError::NotFound);
        }
        self.read(scope)
    }
    fn read(&self, scope: &MemoryScope) -> PanelResult<MemorySnapshot> {
        self.0
            .reader(scope.clone())
            .map_err(memory_error)?
            .snapshot()
            .map_err(memory_error)
    }
}

fn memory_error(error: MemoryError) -> PanelError {
    match error {
        MemoryError::InvalidInput => PanelError::InvalidInput,
        MemoryError::NotFound => PanelError::NotFound,
        _ => PanelError::Unavailable,
    }
}
fn status(value: &PreferenceStatus) -> &'static str {
    match value {
        PreferenceStatus::Confirmed => "confirmed",
        PreferenceStatus::Revoked => "revoked",
    }
}
fn version_effective(preference: &Preference, version: &PreferenceVersion) -> bool {
    version.revision == preference.revision
        && preference.status == PreferenceStatus::Confirmed
        && version.status == PreferenceStatus::Confirmed
}

pub(crate) fn scopes(
    view: &MemoryView,
    after: Option<&MemoryScope>,
    limit: usize,
) -> PanelResult<MemoryScopePage> {
    if !(1..=100).contains(&limit) {
        return Err(PanelError::InvalidInput);
    }
    let items = view
        .scopes()?
        .into_iter()
        .filter(|scope| after.is_none_or(|after| scope > after))
        .take(limit)
        .map(|scope| {
            let snapshot = view.read(&scope)?;
            let count = |wanted: PreferenceStatus| {
                snapshot
                    .preferences
                    .iter()
                    .filter(|preference| preference.status == wanted)
                    .count()
            };
            Ok(MemoryScopeSummary {
                revision: snapshot.revision,
                evidence: snapshot.evidence.len(),
                confirmed: count(PreferenceStatus::Confirmed),
                revoked: count(PreferenceStatus::Revoked),
                scope,
            })
        })
        .collect::<PanelResult<Vec<_>>>()?;
    let next_after = (items.len() == limit).then(|| items.last().expect("nonempty").scope.clone());
    Ok(MemoryScopePage { items, next_after })
}

pub(crate) fn detail(view: &MemoryView, scope: &MemoryScope) -> PanelResult<MemoryDetail> {
    let snapshot = view.snapshot(scope)?;
    let kind = |id: &str| {
        snapshot
            .evidence
            .iter()
            .find(|evidence| evidence.id == id)
            .map_or("missing", |evidence| match evidence.source {
                EvidenceSource::UserStatement { .. } => "user_statement",
                EvidenceSource::CompletedInteraction { .. } => "completed_interaction",
            })
    };
    let mut preferences: Vec<_> = snapshot
        .preferences
        .iter()
        .map(|preference| {
            let (text, text_truncated) = clip(&preference.text, PREFERENCE_TEXT_BYTES);
            let mut history: Vec<_> = preference
                .history
                .iter()
                .map(|version| {
                    let (text, text_truncated) = clip(&version.text, HISTORY_TEXT_BYTES);
                    PreferenceVersionView {
                        revision: version.revision,
                        status: status(&version.status),
                        at_ms: version.at_ms,
                        current: version.revision == preference.revision,
                        text,
                        text_truncated,
                        evidence_id: version.evidence_id.clone(),
                        evidence_kind: kind(&version.evidence_id),
                    }
                })
                .collect();
            history.sort_by_key(|version| std::cmp::Reverse(version.revision));
            PreferenceView {
                id: preference.id.clone(),
                status: status(&preference.status),
                revision: preference.revision,
                effective: preference.status == PreferenceStatus::Confirmed,
                text,
                text_truncated,
                history,
            }
        })
        .collect();
    preferences.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(MemoryDetail {
        scope: snapshot.scope.clone(),
        revision: snapshot.revision,
        evidence: snapshot.evidence.len(),
        preferences,
    })
}

pub(crate) fn evidence(
    view: &MemoryView,
    scope: &MemoryScope,
    id: &str,
) -> PanelResult<MemoryEvidenceDetail> {
    let snapshot = view.snapshot(scope)?;
    let evidence = snapshot
        .evidence
        .iter()
        .find(|evidence| evidence.id == id)
        .ok_or(PanelError::NotFound)?;
    // 用户输入与助手回复分列；助手回复只是当时的历史内容，不升级为用户事实。
    let (kind, user, assistant, turn_id) = match &evidence.source {
        EvidenceSource::UserStatement { text, .. } => ("user_statement", text, None, None),
        EvidenceSource::CompletedInteraction {
            user_text,
            assistant_text,
            turn_id,
            ..
        } => (
            "completed_interaction",
            user_text,
            Some(assistant_text),
            Some(*turn_id),
        ),
    };
    let (user_text, user_text_truncated) = clip(user, EVIDENCE_TEXT_BYTES);
    let (assistant_text, assistant_text_truncated) = assistant.map_or((None, false), |text| {
        let (text, truncated) = clip(text, EVIDENCE_TEXT_BYTES);
        (Some(text), truncated)
    });
    let mut references: Vec<_> = snapshot
        .preferences
        .iter()
        .flat_map(|preference| {
            preference
                .history
                .iter()
                .filter(|version| version.evidence_id == id)
                .map(move |version| EvidenceReference {
                    preference_id: preference.id.clone(),
                    revision: version.revision,
                    current: version.revision == preference.revision,
                    effective: version_effective(preference, version),
                })
        })
        .collect();
    references.sort_by(|a, b| {
        a.preference_id
            .cmp(&b.preference_id)
            .then(a.revision.cmp(&b.revision))
    });
    let references_total = references.len();
    references.truncate(REFERENCE_LIMIT);
    Ok(MemoryEvidenceDetail {
        scope: snapshot.scope.clone(),
        id: evidence.id.clone(),
        revision: evidence.revision,
        at_ms: evidence.at_ms,
        kind,
        user_text,
        user_text_truncated,
        assistant_text,
        assistant_text_truncated,
        turn_id,
        references,
        references_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_memory_api::{
        CompletedInteraction, InteractionEvidence, MemoryResult, MemoryService, PreferenceChange,
    };
    use std::sync::Mutex;

    /// 写入方法一旦被调用就失败测试，确认面板路径只读。
    struct Admin {
        snapshots: Vec<MemorySnapshot>,
        broken: Mutex<bool>,
    }
    struct Reader(MemorySnapshot);
    impl MemoryService for Reader {
        fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
            Ok(self.0.clone())
        }
    }
    impl MemoryAdmin for Admin {
        fn scopes(&self) -> MemoryResult<Vec<MemoryScope>> {
            if *self.broken.lock().unwrap() {
                return Err(MemoryError::Unavailable);
            }
            Ok(self
                .snapshots
                .iter()
                .rev()
                .map(|s| s.scope.clone())
                .collect())
        }
        fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
            // 与真实实现一致：未知作用域返回空快照而不是错误。
            let snapshot = self
                .snapshots
                .iter()
                .find(|s| s.scope == scope)
                .cloned()
                .unwrap_or(MemorySnapshot {
                    scope,
                    revision: 0,
                    evidence: vec![],
                    preferences: vec![],
                });
            Ok(Arc::new(Reader(snapshot)))
        }
        fn import_completed(
            &self,
            _: &MemoryScope,
            _: u64,
            _: CompletedInteraction,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("面板不得导入交互");
        }
        fn update_preference(
            &self,
            _: &MemoryScope,
            _: u64,
            _: PreferenceChange,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("面板不得修改偏好");
        }
    }

    fn scope(user: &str) -> MemoryScope {
        MemoryScope {
            channel: "qq".into(),
            session_id: "session".into(),
            user_id: user.into(),
        }
    }
    fn version(
        revision: u64,
        evidence: &str,
        text: &str,
        status: PreferenceStatus,
    ) -> PreferenceVersion {
        PreferenceVersion {
            revision,
            evidence_id: evidence.into(),
            at_ms: revision * 10,
            text: text.into(),
            status,
        }
    }
    fn fixture() -> MemoryView {
        let long = "偏".repeat(300);
        let confirmed = Preference {
            id: "pref-b".into(),
            text: "先给结论".into(),
            status: PreferenceStatus::Confirmed,
            revision: 2,
            history: vec![
                version(1, "statement", &long, PreferenceStatus::Confirmed),
                version(2, "interaction", "先给结论", PreferenceStatus::Confirmed),
            ],
        };
        let revoked = Preference {
            id: "pref-a".into(),
            text: "旧偏好".into(),
            status: PreferenceStatus::Revoked,
            revision: 2,
            history: vec![
                version(1, "statement", "旧偏好", PreferenceStatus::Confirmed),
                version(2, "gone", "旧偏好", PreferenceStatus::Revoked),
            ],
        };
        let owner = MemorySnapshot {
            scope: scope("user-a"),
            revision: 5,
            evidence: vec![
                InteractionEvidence {
                    id: "statement".into(),
                    revision: 1,
                    at_ms: 10,
                    source: EvidenceSource::UserStatement {
                        message_id: "raw-message-1".into(),
                        text: format!("/remember {}", "长".repeat(1000)),
                    },
                },
                InteractionEvidence {
                    id: "interaction".into(),
                    revision: 3,
                    at_ms: 20,
                    source: EvidenceSource::CompletedInteraction {
                        message_id: "raw-message-2".into(),
                        session_revision: 4,
                        turn_id: 2,
                        user_text: "请简短一点".into(),
                        assistant_text: "好的，我会先给结论。".into(),
                    },
                },
            ],
            preferences: vec![confirmed, revoked],
        };
        let other = MemorySnapshot {
            scope: scope("user-b"),
            revision: 1,
            evidence: vec![],
            preferences: vec![],
        };
        MemoryView::new(Arc::new(Admin {
            snapshots: vec![owner, other],
            broken: Mutex::new(false),
        }))
    }

    #[test]
    fn scopes_are_sorted_paged_and_counted_without_creating_unknown_scopes() {
        let view = fixture();
        let page = scopes(&view, None, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        let first = &page.items[0];
        assert_eq!(first.scope.user_id, "user-a");
        assert_eq!(
            (
                first.revision,
                first.evidence,
                first.confirmed,
                first.revoked
            ),
            (5, 2, 1, 1)
        );
        assert_eq!(page.next_after.as_ref(), Some(&scope("user-a")));
        let rest = scopes(&view, page.next_after.as_ref(), 100).unwrap();
        let users: Vec<_> = rest
            .items
            .iter()
            .map(|s| s.scope.user_id.as_str())
            .collect();
        assert_eq!(users, ["user-b"]);
        assert!(rest.next_after.is_none());
        for limit in [0, 101] {
            assert!(matches!(
                scopes(&view, None, limit),
                Err(PanelError::InvalidInput)
            ));
        }
    }

    #[test]
    fn detail_orders_preferences_and_history_and_marks_effective_versions() {
        let view = fixture();
        let detail = detail(&view, &scope("user-a")).unwrap();
        assert_eq!((detail.revision, detail.evidence), (5, 2));
        let ids: Vec<_> = detail.preferences.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["pref-a", "pref-b"]);
        let revoked = &detail.preferences[0];
        assert_eq!((revoked.status, revoked.effective), ("revoked", false));
        let kinds: Vec<_> = revoked
            .history
            .iter()
            .map(|v| (v.revision, v.evidence_kind))
            .collect();
        assert_eq!(kinds, [(2, "missing"), (1, "user_statement")]);
        let confirmed = &detail.preferences[1];
        assert_eq!((confirmed.status, confirmed.effective), ("confirmed", true));
        let history: Vec<_> = confirmed
            .history
            .iter()
            .map(|v| (v.revision, v.current, v.evidence_kind, v.text_truncated))
            .collect();
        assert_eq!(
            history,
            [
                (2, true, "completed_interaction", false),
                (1, false, "user_statement", true)
            ]
        );
        assert!(confirmed.history[1].text.len() <= HISTORY_TEXT_BYTES);
    }

    #[test]
    fn evidence_separates_user_and_assistant_text_and_lists_references() {
        let view = fixture();
        let statement = evidence(&view, &scope("user-a"), "statement").unwrap();
        assert_eq!(statement.kind, "user_statement");
        assert!(statement.user_text_truncated);
        assert!(statement.user_text.len() <= EVIDENCE_TEXT_BYTES);
        assert!(statement.assistant_text.is_none());
        let references: Vec<_> = statement
            .references
            .iter()
            .map(|r| (r.preference_id.as_str(), r.revision, r.current, r.effective))
            .collect();
        assert_eq!(
            references,
            [("pref-a", 1, false, false), ("pref-b", 1, false, false)]
        );
        assert_eq!(statement.references_total, 2);

        let interaction = evidence(&view, &scope("user-a"), "interaction").unwrap();
        assert_eq!(interaction.kind, "completed_interaction");
        assert_eq!(interaction.user_text, "请简短一点");
        assert_eq!(
            interaction.assistant_text.as_deref(),
            Some("好的，我会先给结论。")
        );
        assert_eq!(interaction.turn_id, Some(2));
        assert!(interaction.references[0].effective);
        let encoded = serde_json::to_string(&interaction).unwrap();
        assert!(!encoded.contains("raw-message"), "消息 ID 不返回");
    }

    #[test]
    fn unknown_scope_and_evidence_are_not_found_and_backend_errors_are_unavailable() {
        let view = fixture();
        assert!(matches!(
            detail(&view, &scope("nobody")),
            Err(PanelError::NotFound)
        ));
        assert!(matches!(
            evidence(&view, &scope("user-b"), "statement"),
            Err(PanelError::NotFound)
        ));
        assert!(matches!(
            evidence(&view, &scope("user-a"), "missing"),
            Err(PanelError::NotFound)
        ));
        let broken = MemoryView::new(Arc::new(Admin {
            snapshots: vec![],
            broken: Mutex::new(true),
        }));
        assert!(matches!(
            scopes(&broken, None, 10),
            Err(PanelError::Unavailable)
        ));
        assert!(matches!(
            detail(&broken, &scope("user-a")),
            Err(PanelError::Unavailable)
        ));
    }
}
