use eve_llm_api::{ChatMessage, ChatRole};
use eve_session_api::*;
fn snapshot() -> SessionSnapshot {
    let turns = (1..=33)
        .map(|id| SessionTurn {
            id,
            input: format!("完整原文{id}"),
            status: SessionTurnStatus::Completed {
                messages: vec![
                    ChatMessage::text(ChatRole::User, format!("完整原文{id}")),
                    ChatMessage::text(ChatRole::Assistant, format!("完整回复{id}")),
                ],
            },
        })
        .collect();
    SessionSnapshot {
        key: SessionKey::new("s", "u").unwrap(),
        revision: 66,
        turns,
    }
}
#[test]
fn snapshot_adapter_preserves_originals_and_page_boundaries() {
    let source = snapshot();
    let mut cursor = SnapshotTurnCursor::new(source.clone()).unwrap();
    assert_eq!(cursor.summary().revision, 66);
    let mut turns = vec![];
    loop {
        let page = cursor.next_page().unwrap();
        assert!(page.len() <= MAX_SESSION_TURN_PAGE);
        if page.is_empty() {
            break;
        }
        turns.extend(page);
    }
    assert_eq!(turns, source.turns);
    assert!(cursor.next_page().unwrap().is_empty());
}
struct Legacy;
impl SessionService for Legacy {
    fn snapshot(&self, _: &SessionKey) -> SessionResult<Option<SessionSnapshot>> {
        Ok(Some(snapshot()))
    }
    fn begin(&self, _: SessionInput) -> SessionResult<StartedTurn> {
        Err(SessionError::Unavailable)
    }
    fn complete(&self, _: &TurnLease, _: Vec<ChatMessage>) -> SessionResult<()> {
        Err(SessionError::Unavailable)
    }
    fn fail(&self, _: &TurnLease, _: SessionFailure) -> SessionResult<()> {
        Err(SessionError::Unavailable)
    }
}
#[test]
fn defaults_reject_foreign_owner_and_invalid_snapshot_before_returning_body() {
    assert!(matches!(
        Legacy.scan_turns(&SessionKey::new("s", "other").unwrap()),
        Err(SessionError::OwnerMismatch)
    ));
    let mut wrong = snapshot();
    wrong.revision = 1;
    assert!(SnapshotTurnCursor::new(wrong).is_err());
    assert_eq!(
        Legacy
            .scan_turns(&SessionKey::new("s", "u").unwrap())
            .unwrap()
            .unwrap()
            .next_page()
            .unwrap()
            .len(),
        MAX_SESSION_TURN_PAGE
    );
}
