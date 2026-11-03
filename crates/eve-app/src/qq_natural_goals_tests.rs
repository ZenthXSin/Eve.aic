use super::*;
use eve_developer_api::{ActionReviewFuture, ActionReviewVerdict, ReviewRisk};
use eve_llm_api::{ChatMessage, ContextScope};
use serde_json::json;
use std::{collections::BTreeSet, path::Path, sync::Mutex};

pub(crate) struct Reviewer {
    pub(crate) requests: Mutex<Vec<ActionReviewRequest>>,
    pub(crate) verdict: Result<ActionReviewVerdict, String>,
    pub(crate) during: Option<Box<dyn Fn() + Send + Sync>>,
}
impl ActionReviewer for Reviewer {
    fn review(&self, request: ActionReviewRequest) -> ActionReviewFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move {
            if let Some(during) = &self.during {
                during();
            }
            self.verdict.clone()
        })
    }
}
pub(crate) fn reviewer() -> Arc<Reviewer> {
    Arc::new(Reviewer {
        requests: Mutex::new(vec![]),
        verdict: Ok(ActionReviewVerdict {
            risk: ReviewRisk::Medium,
            authorization: ReviewAuthorization::High,
            allow: true,
            rationale: "固定许可用于边界验收".into(),
        }),
        during: None,
    })
}
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
fn words() -> Vec<String> {
    (0..6)
        .map(|i| format!("第{i}条完整原话：{}：尾部{i}", "正文".repeat(1500)))
        .collect()
}
fn ledger(directory: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(directory.join("business-permissions.json")).unwrap())
        .unwrap()
}

#[tokio::test]
async fn complete_review_and_trace_survive_restart_without_permission_grant() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let reviewer = reviewer();
    let boundary = Review {
        admin: admin.clone(),
        reviewer: Some(reviewer.clone()),
    };
    let call = call(
        "manage_goal",
        "save",
        json!({"action":"create","text":"整理下一步"}),
    );
    let words = words();
    let previous = (0..12)
        .map(|i| {
            super::tests::call(
                "read_file",
                &format!("read-{i}"),
                json!({"path":format!("file-{i}"),"details":"d".repeat(2000)}),
            )
        })
        .collect();
    let context = context(&call, "administrator", words.clone()).with_conversation(Arc::new(vec![
        ChatMessage::assistant_tool_calls(previous).unwrap(),
    ]));
    let evidence = json!({"arguments":call.arguments,"current_goal":null});
    boundary
        .approve("manage_goal", &call, &context, evidence.clone())
        .await
        .unwrap();
    let requests = reviewer.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.user_messages, words);
    assert_eq!(request.recent_actions.len(), 8);
    assert!(request.recent_actions[0].starts_with("read-4 read_file "));
    assert!(request.recent_actions[7].starts_with("read-11 read_file "));
    assert!(
        request
            .recent_actions
            .iter()
            .all(|action| action.len() <= 1200)
    );
    let action: Value = serde_json::from_str(&request.arguments[0]).unwrap();
    assert_eq!(action["operation"], evidence);
    assert_eq!(action["call_id"], call.id);
    assert_eq!(action["turn_id"], 17);
    assert_eq!(action["trusted_scope"]["user_id"], "administrator");
    let disk = ledger(dir.path());
    assert_eq!(disk["audit"][0]["user_messages"], json!(words));
    assert_eq!(disk["audit"][0]["outcome"], "review_allowed");
    assert_eq!(
        serde_json::from_str::<Value>(disk["audit"][0]["target"].as_str().unwrap()).unwrap(),
        serde_json::to_value(request).unwrap()
    );
    assert_eq!(disk["grants"], json!({}));
    drop(requests);
    let reopened = super::tests::admin(dir.path());
    assert_eq!(reopened.business.revision().unwrap(), 1);
    assert!(reopened.business_permitted(context.scope().unwrap()));
    assert_eq!(ledger(dir.path()), disk);
}

#[tokio::test]
async fn weak_confirmation_and_unavailable_review_are_audited_without_grant() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let call = call("manage_plan", "confirm", json!({"action":"confirm"}));
    let context = context(&call, "administrator", vec!["确认上述计划".into()]);
    let mut reviewer = reviewer();
    Arc::get_mut(&mut reviewer)
        .unwrap()
        .verdict
        .as_mut()
        .unwrap()
        .authorization = ReviewAuthorization::Medium;
    let boundary = Review {
        admin: admin.clone(),
        reviewer: Some(reviewer.clone()),
    };
    assert!(
        boundary
            .approve("manage_plan", &call, &context, json!({"steps":["研究"]}))
            .await
            .is_err()
    );
    let boundary = Review {
        admin: admin.clone(),
        reviewer: None,
    };
    assert!(
        boundary
            .approve("manage_plan", &call, &context, json!({}))
            .await
            .is_err()
    );
    let disk = ledger(dir.path());
    assert_eq!(disk["audit"][0]["outcome"], "review_denied");
    assert_eq!(disk["audit"][1]["outcome"], "review_unavailable");
    assert_eq!(disk["grants"], json!({}));
}

#[tokio::test]
async fn identity_delegation_evidence_and_capacity_fail_before_review() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let reviewer = reviewer();
    let boundary = Review {
        admin: admin.clone(),
        reviewer: Some(reviewer.clone()),
    };
    let call = call("manage_goal", "create", json!({"action":"create"}));
    let valid = context(&call, "administrator", vec!["记住待办".into()]);
    let invalid = [
        context(&call, "stranger", vec!["记住待办".into()]),
        valid.clone().with_delegated(true),
        valid.clone().with_call_id("forged".into()),
        valid.clone().with_review_user_messages(Arc::new(vec![])),
        valid.clone().with_review_user_messages(Arc::new(vec![
            "x".repeat(eve_developer_api::MAX_ACTION_REVIEW_BYTES),
        ])),
    ];
    for context in invalid {
        assert!(
            boundary
                .approve("manage_goal", &call, &context, json!({}))
                .await
                .is_err()
        );
    }
    valid.cancellation_handle().cancel();
    assert!(
        boundary
            .approve("manage_goal", &call, &valid, json!({}))
            .await
            .is_err()
    );
    assert!(reviewer.requests.lock().unwrap().is_empty());
    assert_eq!(admin.business.revision().unwrap(), 0);
}

#[tokio::test]
async fn revoke_during_review_blocks_the_approved_action() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let call = call("manage_goal", "create", json!({"action":"create"}));
    let context = context(&call, "administrator", vec!["保存待办".into()]);
    let mut reviewer = reviewer();
    let revoke_admin = admin.clone();
    let scope = context.scope().unwrap().clone();
    Arc::get_mut(&mut reviewer).unwrap().during = Some(Box::new(move || {
        revoke_admin
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
    }));
    let boundary = Review {
        admin: admin.clone(),
        reviewer: Some(reviewer.clone()),
    };
    assert!(
        boundary
            .approve("manage_goal", &call, &context, json!({}))
            .await
            .is_err()
    );
    assert_eq!(reviewer.requests.lock().unwrap().len(), 1);
    assert!(!admin.business_permitted(context.scope().unwrap()));
    assert_eq!(ledger(dir.path())["audit"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn audit_write_failure_and_corruption_preserve_bytes_and_reject() {
    let dir = tempfile::tempdir().unwrap();
    let admin = admin(dir.path());
    let reviewer = reviewer();
    let call = call("manage_goal", "create", json!({"action":"create"}));
    let context = context(&call, "administrator", vec!["保存待办".into()]);
    let path = dir.path().join("business-permissions.json");
    std::fs::create_dir(&path).unwrap();
    let boundary = Review {
        admin,
        reviewer: Some(reviewer.clone()),
    };
    assert!(
        boundary
            .approve("manage_goal", &call, &context, json!({}))
            .await
            .is_err()
    );
    assert!(path.is_dir());
    assert_eq!(reviewer.requests.lock().unwrap().len(), 1);
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
