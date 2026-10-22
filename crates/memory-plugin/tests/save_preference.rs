use eve_dialogue_tools::{OwnerCheck, SavePreferenceTool, Services};
use eve_kernel::{Kernel, KernelServices};
use eve_llm_api::{ContextScope, Tool, ToolCall, ToolExecutionContext, ToolExecutionError};
use eve_memory_api::*;
use eve_memory_plugin::{MEMORY_STATE_KEY, MemoryController, MemoryPlugin};
use eve_plugin_api::{PluginId, StateStore};
use eve_session_api::SESSION_SERVICE_ID;
use eve_session_plugin::SessionPlugin;
use serde_json::json;
use std::sync::Arc;

fn owner_plugin() -> PluginId {
    PluginId::new(MEMORY_PLUGIN_ID).unwrap()
}

fn scope(session: &str, user: &str) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: session.into(),
        user_id: user.into(),
    }
}

async fn open(state: Arc<dyn StateStore>) -> (Kernel, MemoryController) {
    let backends = KernelServices {
        state,
        ..KernelServices::default()
    };
    let kernel = Kernel::with_services(backends);
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel
        .register(Box::new(SessionPlugin::new().unwrap()))
        .unwrap();
    kernel.start_all().await.unwrap();
    let _ = (owner_plugin(), MEMORY_STATE_KEY, SESSION_SERVICE_ID);
    (kernel, admin)
}

fn tool(admin: MemoryController, owner: bool) -> SavePreferenceTool {
    SavePreferenceTool {
        services: Arc::new(Services {
            memory: Some(Arc::new(admin)),
            owner_check: Some(Arc::new(OwnerCheck(move |_: &ContextScope| owner))),
            ..Services::default()
        }),
    }
}

fn context(session: &str, user: &str) -> ToolExecutionContext {
    ToolExecutionContext::new().with_scope(Some(ContextScope {
        session_id: session.into(),
        user_id: user.into(),
    }))
}

fn call(level: &str, text: &str, id: Option<&str>) -> ToolCall {
    let mut arguments = json!({"scope": level, "text": text});
    if let Some(id) = id {
        arguments["id"] = json!(id);
    }
    ToolCall {
        id: "call-1".into(),
        name: "save_preference".into(),
        arguments,
    }
}

fn confirmed<'a>(snapshot: &'a MemorySnapshot, text: &str) -> Vec<&'a Preference> {
    snapshot
        .preferences
        .iter()
        .filter(|preference| {
            preference.status == PreferenceStatus::Confirmed && preference.text == text
        })
        .collect()
}

#[tokio::test]
async fn session_user_and_global_write_only_their_scopes() {
    let directory = tempfile::tempdir().unwrap();
    let store = eve_kernel::backends::FileStateStore::open(directory.path()).unwrap();
    let (kernel, admin) = open(Arc::new(store)).await;
    let session_tool = tool(admin.clone(), false);
    let saved = session_tool
        .execute(call("session", "短句", None), context("here", "alice"))
        .await
        .unwrap();
    assert_eq!(saved["sessions_written"], 1);
    assert!(saved["failed"].as_array().unwrap().is_empty());
    let id = saved["written"][0]["preference_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(saved["written"][0]["revision"], 1);
    assert_eq!(
        confirmed(
            &admin
                .reader(scope("here", "alice"))
                .unwrap()
                .snapshot()
                .unwrap(),
            "短句"
        )
        .len(),
        1
    );
    assert!(
        admin
            .reader(scope("there", "alice"))
            .unwrap()
            .snapshot()
            .unwrap()
            .preferences
            .is_empty()
    );

    admin
        .update_preference(
            &scope("there", "alice"),
            0,
            PreferenceChange {
                operation_id: "seed-there".into(),
                at_ms: 1,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "seed-there-evidence".into(),
                    message_id: "seed-there-message".into(),
                    text: "占位".into(),
                    at_ms: 1,
                }),
                action: PreferenceAction::Confirm {
                    id: "seed-there-preference".into(),
                    text: "占位".into(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &scope("group", "bob"),
            0,
            PreferenceChange {
                operation_id: "seed-bob".into(),
                at_ms: 1,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "seed-bob-evidence".into(),
                    message_id: "seed-bob-message".into(),
                    text: "别人".into(),
                    at_ms: 1,
                }),
                action: PreferenceAction::Confirm {
                    id: "seed-bob-preference".into(),
                    text: "别人".into(),
                },
            },
        )
        .unwrap();
    admin
        .update_preference(
            &MemoryScope {
                channel: "web".into(),
                session_id: "web-session".into(),
                user_id: "alice".into(),
            },
            0,
            PreferenceChange {
                operation_id: "seed-web".into(),
                at_ms: 1,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "seed-web-evidence".into(),
                    message_id: "seed-web-message".into(),
                    text: "网页".into(),
                    at_ms: 1,
                }),
                action: PreferenceAction::Confirm {
                    id: "seed-web-preference".into(),
                    text: "网页".into(),
                },
            },
        )
        .unwrap();

    let user_saved = session_tool
        .execute(call("user", "同一用户", None), context("here", "alice"))
        .await
        .unwrap();
    assert_eq!(user_saved["sessions_written"], 2);
    assert!(user_saved["failed"].as_array().unwrap().is_empty());
    let sessions: Vec<_> = user_saved["written"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["session_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sessions, vec!["here".to_owned(), "there".to_owned()]);
    assert_eq!(
        confirmed(
            &admin
                .reader(scope("group", "bob"))
                .unwrap()
                .snapshot()
                .unwrap(),
            "同一用户"
        )
        .len(),
        0
    );
    assert_eq!(
        confirmed(
            &admin
                .reader(MemoryScope {
                    channel: "web".into(),
                    session_id: "web-session".into(),
                    user_id: "alice".into(),
                })
                .unwrap()
                .snapshot()
                .unwrap(),
            "同一用户"
        )
        .len(),
        0
    );

    let denied = session_tool
        .execute(call("global", "全体", None), context("here", "alice"))
        .await
        .unwrap_err();
    assert!(matches!(denied, ToolExecutionError::Failed(message) if message.contains("只有主人")));
    assert!(
        confirmed(
            &admin
                .reader(scope("group", "bob"))
                .unwrap()
                .snapshot()
                .unwrap(),
            "全体"
        )
        .is_empty()
    );

    let owner_tool = tool(admin.clone(), true);
    let global = owner_tool
        .execute(call("global", "全体", None), context("here", "alice"))
        .await
        .unwrap();
    assert_eq!(global["sessions_written"], 3);
    assert!(global["failed"].as_array().unwrap().is_empty());
    for (session, user) in [("here", "alice"), ("there", "alice"), ("group", "bob")] {
        assert_eq!(
            confirmed(
                &admin
                    .reader(scope(session, user))
                    .unwrap()
                    .snapshot()
                    .unwrap(),
                "全体"
            )
            .len(),
            1
        );
    }
    assert!(
        confirmed(
            &admin
                .reader(MemoryScope {
                    channel: "web".into(),
                    session_id: "web-session".into(),
                    user_id: "alice".into(),
                })
                .unwrap()
                .snapshot()
                .unwrap(),
            "全体"
        )
        .is_empty()
    );

    let missing = owner_tool
        .execute(
            call("session", "不会新建", Some("qq-memory-missing")),
            context("here", "alice"),
        )
        .await
        .unwrap();
    assert_eq!(missing["sessions_written"], 0, "{missing}");
    assert!(
        missing["failed"][0]["error"]
            .as_str()
            .unwrap()
            .contains("没有这条偏好"),
        "{missing}"
    );

    let before = admin
        .reader(scope("here", "alice"))
        .unwrap()
        .snapshot()
        .unwrap()
        .revision;
    let corrected = owner_tool
        .execute(
            call("session", "修正后", Some(&id)),
            context("here", "alice"),
        )
        .await
        .unwrap();
    assert_eq!(corrected["sessions_written"], 1);
    assert_eq!(corrected["written"][0]["preference_id"], id);
    assert!(corrected["written"][0]["revision"].as_u64().unwrap() > 1);
    let after = admin
        .reader(scope("here", "alice"))
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(after.revision > before);
    let preference = after.preferences.iter().find(|item| item.id == id).unwrap();
    assert_eq!(preference.text, "修正后");
    assert_eq!(preference.status, PreferenceStatus::Confirmed);
    assert!(preference.revision >= 2);

    for text in ["", " 空格", "换行\n", &"长".repeat(2049)] {
        let error = owner_tool
            .execute(call("session", text, None), context("here", "alice"))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolExecutionError::Failed(_)));
    }
    kernel.stop_all().await.unwrap();
}
