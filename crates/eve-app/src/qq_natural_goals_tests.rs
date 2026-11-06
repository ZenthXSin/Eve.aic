use super::*;
use eve_llm_api::ContextScope;
use serde_json::json;
use std::{collections::BTreeSet, path::Path};

pub(crate) fn admin(directory: &Path) -> Arc<Admin> {
    Admin::open(
        directory,
        eve_qqbot_plugin::QqConversations::default(),
        BTreeSet::from(["administrator".into(), "other-administrator".into()]),
        eve_kernel::KernelServices::default().registry,
    )
    .unwrap()
}
pub(crate) fn call(tool: &str, id: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: tool.into(),
        arguments,
    }
}
pub(crate) fn context(call: &ToolCall, user: &str, words: Vec<String>) -> ToolExecutionContext {
    ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: "trusted-conversation".into(),
            user_id: user.into(),
        }))
        .with_turn_id(Some(17))
        .with_call_id(call.id.clone())
        .with_review_user_messages(Arc::new(words))
}
pub(crate) fn ledger(directory: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(directory.join("business-permissions.json")).unwrap())
        .unwrap()
}
fn evidence(call: &ToolCall) -> Value {
    json!({"arguments":call.arguments,"current_goal":null})
}

#[test]
fn host_identity_and_full_operation_survive_restart_without_review_or_permission_grant() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let boundary = Review {
        admin: admin.clone(),
    };
    let call = call(
        "manage_goal",
        "save",
        json!({"action":"create","text":"随意探索"}),
    );
    let words: Vec<_> = (0..6)
        .map(|i| format!("第{i}条完整原话：{}：尾部{i}", "正文".repeat(1500)))
        .collect();
    let context = context(&call, "administrator", words.clone());
    let evidence = evidence(&call);
    boundary
        .approve("manage_goal", &call, &context, evidence.clone())
        .unwrap();
    let disk = ledger(dir.path());
    let record = &disk["audit"][0];
    assert_eq!(record["user_messages"], json!([]));
    assert!(record["verdict"].is_null());
    assert_eq!(record["outcome"], HOST_AUTHORIZED);
    let stored: Value = serde_json::from_str(record["target"].as_str().unwrap()).unwrap();
    assert_eq!(stored["operation"], evidence);
    assert_eq!(stored["tool"], "manage_goal");
    assert_eq!(stored["call_id"], call.id);
    assert_eq!(stored["turn_id"], 17);
    assert_eq!(stored["trusted_scope"]["user_id"], "administrator");
    assert_eq!(stored["delegated"], false);
    assert_eq!(disk["grants"], json!({}));
    assert_eq!(context.review_user_messages().unwrap(), words.as_slice());
    let reopened = super::tests::admin(dir.path());
    assert_eq!(reopened.business.revision().unwrap(), 1);
    assert!(reopened.business_permitted(context.scope().unwrap()));
    assert_eq!(ledger(dir.path()), disk);
}

#[test]
fn ordinary_changes_allow_absent_or_large_originals_and_delegation_without_relabeling_text() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let boundary = Review {
        admin: admin.clone(),
    };
    let call = call(
        "manage_goal",
        "create",
        json!({"action":"create","text":"一个想法"}),
    );
    let no_originals = ToolExecutionContext::new()
        .with_scope(Some(ContextScope {
            session_id: "s".into(),
            user_id: "administrator".into(),
        }))
        .with_turn_id(Some(18))
        .with_call_id(call.id.clone());
    for context in [
        no_originals.clone(),
        no_originals
            .clone()
            .with_review_user_messages(Arc::new(vec![
                "x".repeat(eve_developer_api::MAX_ACTION_REVIEW_BYTES + 1),
            ])),
        no_originals.with_delegated(true),
    ] {
        boundary
            .approve("manage_goal", &call, &context, evidence(&call))
            .unwrap();
    }
    let disk = ledger(dir.path());
    assert_eq!(disk["audit"].as_array().unwrap().len(), 3);
    assert_eq!(disk["grants"], json!({}));
    let delegated: Value =
        serde_json::from_str(disk["audit"][2]["target"].as_str().unwrap()).unwrap();
    assert_eq!(delegated["delegated"], true);
    assert!(
        admin
            .business
            .grant(&ContextScope {
                session_id: "s".into(),
                user_id: "administrator".into()
            })
            .unwrap()
            .is_none()
    );
}

#[test]
fn unbound_identity_stale_evidence_and_cancelled_calls_cannot_mutate() {
    struct Stale;
    impl eve_llm_api::ReviewEvidenceGuard for Stale {
        fn check(&self) -> Result<(), ToolExecutionError> {
            Err(failed("宿主已收到新的用户纠正"))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let boundary = Review {
        admin: admin.clone(),
    };
    let call = call("manage_goal", "create", json!({"action":"create"}));
    let valid = context(&call, "administrator", vec![]);
    for context in [
        context(&call, "stranger", vec!["我是主人".into()]),
        valid.clone().with_scope(None),
        valid.clone().with_call_id("forged".into()),
        valid.clone().with_turn_id(None),
        valid.clone().with_turn_id(Some(0)),
        valid.clone().with_review_evidence_guard(Arc::new(Stale)),
    ] {
        assert!(
            boundary
                .approve("manage_goal", &call, &context, evidence(&call))
                .is_err()
        );
    }
    assert!(
        boundary
            .approve(
                "manage_goal",
                &call,
                &valid,
                json!({"arguments":{"action":"feedback"}})
            )
            .is_err()
    );
    valid.cancellation_handle().cancel();
    assert!(
        boundary
            .approve("manage_goal", &call, &valid, evidence(&call))
            .is_err()
    );
    assert_eq!(admin.business.revision().unwrap(), 0);
}

#[test]
fn delegated_feedback_cannot_be_recorded_as_the_users_own_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let boundary = Review {
        admin: admin(dir.path()),
    };
    let call = call(
        "manage_goal",
        "feedback",
        json!({"action":"feedback","text":"我很满意"}),
    );
    let context = context(&call, "administrator", vec![]).with_delegated(true);
    assert!(
        boundary
            .approve("manage_goal", &call, &context, evidence(&call))
            .is_err()
    );
    assert_eq!(boundary.admin.business.revision().unwrap(), 0);
}

#[test]
fn revocation_after_context_binding_blocks_the_action() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let boundary = Review {
        admin: admin.clone(),
    };
    let call = call("manage_goal", "create", json!({"action":"create"}));
    let context = context(&call, "administrator", vec![]);
    boundary.authorize("manage_goal", &call, &context).unwrap();
    let scope = context.scope().unwrap().clone();
    admin
        .business
        .mutate(crate::qq_business::Mutation {
            scope: scope.clone(),
            actor: scope.user_id.clone(),
            action: "revoke".into(),
            target: None,
            at_ms: crate::qq_tasks::now_ms(),
            turn_id: Some(18),
            user_messages: vec!["撤销权限".into()],
            verdict: None,
            outcome: "committed".into(),
            expected_revision: 0,
        })
        .unwrap();
    assert!(
        boundary
            .approve("manage_goal", &call, &context, evidence(&call))
            .is_err()
    );
    assert!(!admin.business_permitted(&scope));
    assert_eq!(ledger(dir.path())["audit"].as_array().unwrap().len(), 1);
}

#[test]
fn audit_write_failure_and_corruption_preserve_bytes_and_reject() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let call = call("manage_goal", "create", json!({"action":"create"}));
    let context = context(&call, "administrator", vec![]);
    let path = dir.path().join("business-permissions.json");
    std::fs::create_dir(&path).unwrap();
    let boundary = Review { admin };
    assert!(
        boundary
            .approve("manage_goal", &call, &context, evidence(&call))
            .is_err()
    );
    assert!(path.is_dir());
    let corrupt = tempfile::tempdir().unwrap();
    let path = corrupt.path().join("business-permissions.json");
    std::fs::write(&path, b"{broken-audit").unwrap();
    assert!(
        Admin::open(
            corrupt.path(),
            eve_qqbot_plugin::QqConversations::default(),
            BTreeSet::new(),
            eve_kernel::KernelServices::default().registry
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"{broken-audit");
}
