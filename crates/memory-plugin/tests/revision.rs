//! 轻量修订不创建未知范围；跟随真实保存并保持旧句柄关闭。
use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::{ChatMessage, ChatRole};
use eve_memory_api::*;
use eve_memory_plugin::MemoryPlugin;
use eve_session_api::{SessionKey, SessionSnapshot, SessionTurn, SessionTurnStatus};
use std::sync::Arc;

#[tokio::test]
async fn revision_follows_real_updates_without_creating_unknown_scopes_or_reactivating_old_readers()
{
    let state = Arc::new(MemoryStateStore::default());
    let kernel = Kernel::with_services(KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    });
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    let scope = MemoryScope {
        channel: "qq".into(),
        session_id: "revision".into(),
        user_id: "alice".into(),
    };
    let reader = admin.reader(scope.clone()).unwrap();
    assert_eq!(reader.revision().unwrap(), 0);
    assert!(admin.scopes().unwrap().is_empty());
    assert!(admin.revisions("qq").unwrap().is_empty());
    admin
        .import_completed(
            &scope,
            0,
            CompletedInteraction {
                evidence_id: "e1".into(),
                message_id: "m1".into(),
                at_ms: 1,
                turn_id: 1,
                snapshot: SessionSnapshot {
                    key: SessionKey::new(&scope.session_id, &scope.user_id).unwrap(),
                    revision: 2,
                    turns: vec![SessionTurn {
                        id: 1,
                        input: "问题".into(),
                        status: SessionTurnStatus::Completed {
                            messages: vec![
                                ChatMessage::text(ChatRole::User, "问题"),
                                ChatMessage::text(ChatRole::Assistant, "回复"),
                            ],
                        },
                    }],
                },
            },
        )
        .unwrap();
    assert_eq!(reader.revision().unwrap(), 1);
    admin
        .update_preference(
            &scope,
            1,
            PreferenceChange {
                operation_id: "op1".into(),
                at_ms: 2,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "statement".into(),
                    message_id: "m2".into(),
                    text: "/remember 简短回答".into(),
                    at_ms: 2,
                }),
                action: PreferenceAction::Confirm {
                    id: "p1".into(),
                    text: "简短回答".into(),
                },
            },
        )
        .unwrap();
    assert_eq!(reader.revision().unwrap(), 2);
    assert_eq!(admin.revisions("qq").unwrap(), vec![(scope.clone(), 2)]);
    assert!(admin.revisions("console").unwrap().is_empty());
    assert_eq!(admin.revisions(""), Err(MemoryError::InvalidInput));
    kernel.stop_all().await.unwrap();
    assert_eq!(reader.revision(), Err(MemoryError::Unavailable));
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    assert_eq!(admin.reader(scope).unwrap().revision().unwrap(), 2);
    assert_eq!(reader.revision(), Err(MemoryError::Unavailable));
    kernel.stop_all().await.unwrap();
}
