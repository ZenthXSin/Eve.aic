use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_outreach_api::*;
use eve_outreach_plugin::{OutreachController, OutreachPlugin};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// 真实文件库；可注入提交前失败。
struct Store {
    inner: FileStateStore,
    fail_before: AtomicBool,
}
impl Store {
    fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_before: AtomicBool::new(false),
        })
    }
    fn namespace() -> PluginId {
        PluginId::new(OUTREACH_PLUGIN_ID).unwrap()
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.inner
            .get(&Self::namespace(), OUTREACH_STATE_KEY)
            .unwrap()
    }
    fn saved(&self) -> Value {
        self.raw()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
            .unwrap_or(Value::Null)
    }
    fn overwrite(&self, bytes: &[u8]) {
        self.inner
            .set(
                &Self::namespace(),
                OUTREACH_STATE_KEY.into(),
                bytes.to_vec(),
            )
            .unwrap();
    }
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail_before.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)
    }
}

async fn open(state: Arc<Store>) -> PluginResult<(Kernel, OutreachController)> {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = OutreachPlugin::new()?;
    let admin = plugin.controller();
    kernel.register(Box::new(plugin))?;
    kernel.start_all().await?;
    Ok((kernel, admin))
}

fn facts() -> Vec<Fact> {
    vec![
        Fact {
            kind: FactKind::UserQuote,
            text: "我喜欢这个游戏的模组".into(),
        },
        Fact {
            kind: FactKind::Progress,
            text: "做了一个新方块，实际加载并通过 4 项探测".into(),
        },
    ]
}

fn milestone() -> Milestone {
    Milestone {
        practice_run_id: "practice-1".into(),
        skill_id: Some("skill-1".into()),
    }
}

/// 从准入到撰写完成，返回 Pending 邀请。
fn pending(admin: &OutreachController, owner: &str, goal: &str, at: u64) -> Invitation {
    let invitation = admin
        .begin(owner, goal, milestone(), facts(), "composer:v1", at)
        .unwrap()
        .unwrap();
    admin
        .record_composition(
            &invitation.id,
            at + 1,
            Ok("要不要一起做一个新方块？".into()),
        )
        .unwrap()
}

fn passive(id: &str) -> DeliveryChannel {
    DeliveryChannel::Passive {
        message_id: id.into(),
    }
}

/// 对用户某条消息判断为“邀请”，随后才能被动附带。
fn invite(admin: &OutreachController, id: &str, message: &str, at: u64) {
    admin.begin_judgement(id, message, at).unwrap();
    admin
        .record_judgement(id, message, at + 1, Ok(Verdict::Invite))
        .unwrap();
}

#[tokio::test]
async fn each_stage_is_saved_before_the_next_effect_and_goals_are_invited_once() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await.unwrap();
    let invitation = admin
        .begin(
            "owner-a",
            "goal-1",
            milestone(),
            facts(),
            "composer:v1",
            100,
        )
        .unwrap()
        .unwrap();
    assert_eq!(store.saved()["invitations"][0]["status"], "Composing");
    assert!(
        admin
            .begin(
                "owner-a",
                "goal-1",
                milestone(),
                facts(),
                "composer:v1",
                101
            )
            .unwrap()
            .is_none(),
        "同一学习目标只邀请一次"
    );
    let composed = admin
        .record_composition(&invitation.id, 110, Ok("一起做点什么吧？".into()))
        .unwrap();
    assert_eq!(composed.status, InvitationStatus::Pending);
    assert_eq!(
        admin.claim(&invitation.id, 115, passive("msg-0")).err(),
        Some(OutreachError::Conflict),
        "没有判断不能被动附带"
    );
    admin.begin_judgement(&invitation.id, "msg-0", 115).unwrap();
    assert_eq!(
        store.saved()["invitations"][0]["judgements"][0]["outcome"],
        Value::Null
    );
    admin
        .record_judgement(&invitation.id, "msg-0", 116, Ok(Verdict::NotNow))
        .unwrap();
    assert_eq!(
        admin.claim(&invitation.id, 117, passive("msg-0")).err(),
        Some(OutreachError::Conflict),
        "判断为此刻不合适时不附带"
    );
    assert_eq!(
        admin.begin_judgement(&invitation.id, "msg-0", 118).err(),
        Some(OutreachError::Conflict),
        "同一条消息只判断一次"
    );
    admin.begin_judgement(&invitation.id, "msg-1", 118).unwrap();
    admin
        .record_judgement(&invitation.id, "msg-1", 119, Ok(Verdict::Invite))
        .unwrap();
    let claimed = admin.claim(&invitation.id, 120, passive("msg-1")).unwrap();
    assert_eq!(claimed.status, InvitationStatus::Delivering);
    assert_eq!(store.saved()["invitations"][0]["status"], "Delivering");
    assert_eq!(
        store.saved()["invitations"][0]["attempts"][0]["channel"],
        json!({"passive": {"message_id": "msg-1"}})
    );
    let delivered = admin
        .record_delivery(
            &invitation.id,
            130,
            AttemptResult::Sent {
                platform_message_id: Some("out-1".into()),
            },
        )
        .unwrap();
    assert_eq!(delivered.status, InvitationStatus::Delivered);
    assert_eq!(delivered.delivered_at_ms, Some(130));
    assert_eq!(
        admin.claim(&invitation.id, 140, passive("msg-2")).err(),
        Some(OutreachError::Conflict),
        "已送达的邀请不再投递"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn undelivered_attempts_return_to_pending_proactive_is_tried_once_and_attempts_are_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await.unwrap();
    let invitation = pending(&admin, "owner-a", "goal-1", 100);
    let other = pending(&admin, "owner-a", "goal-2", 100);

    admin
        .claim(&invitation.id, 200, DeliveryChannel::Proactive)
        .unwrap();
    assert_eq!(
        admin
            .claim(&other.id, 201, DeliveryChannel::Proactive)
            .err(),
        Some(OutreachError::Conflict),
        "同一用户一次只发送一条"
    );
    assert_eq!(
        admin.begin_judgement(&other.id, "msg-1", 201).err(),
        Some(OutreachError::Conflict),
        "发送中不判断同一用户的其他邀请"
    );
    let refused = admin
        .record_delivery(
            &invitation.id,
            210,
            AttemptResult::Failed {
                http_status: Some(400),
                biz_code: Some(22009),
            },
        )
        .unwrap();
    assert_eq!(
        refused.status,
        InvitationStatus::Pending,
        "平台拒绝时保持待投递"
    );
    assert_eq!(
        admin
            .claim(&invitation.id, 220, DeliveryChannel::Proactive)
            .err(),
        Some(OutreachError::Conflict),
        "每条邀请至多主动尝试一次"
    );
    let mut at = 230;
    for attempt in 2..=MAX_ATTEMPTS {
        invite(&admin, &invitation.id, &format!("msg-{attempt}"), at);
        at += 2;
        admin
            .claim(&invitation.id, at, passive(&format!("msg-{attempt}")))
            .unwrap();
        let result = admin
            .record_delivery(&invitation.id, at + 1, AttemptResult::NotSent)
            .unwrap();
        at += 10;
        let expected = if attempt == MAX_ATTEMPTS {
            InvitationStatus::Failed(OutreachFailure::Exhausted)
        } else {
            InvitationStatus::Pending
        };
        assert_eq!(result.status, expected);
    }
    assert_eq!(
        admin
            .record_delivery(&invitation.id, at, AttemptResult::Unknown)
            .err(),
        Some(OutreachError::InvalidInput),
        "结果未知只能由重启核对写入"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_composing_and_marks_in_flight_delivery_unknown_without_resending() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await.unwrap();
    let composing = admin
        .begin(
            "owner-a",
            "goal-1",
            milestone(),
            facts(),
            "composer:v1",
            100,
        )
        .unwrap()
        .unwrap();
    let sending = pending(&admin, "owner-b", "goal-2", 100);
    invite(&admin, &sending.id, "msg-1", 110);
    admin.claim(&sending.id, 120, passive("msg-1")).unwrap();
    let judging = pending(&admin, "owner-c", "goal-3", 100);
    admin.begin_judgement(&judging.id, "msg-9", 130).unwrap();
    kernel.stop_all().await.unwrap();

    let (kernel, admin) = open(store.clone()).await.unwrap();
    let snapshot = admin.snapshot().unwrap();
    let find = |id: &str| {
        snapshot
            .invitations
            .iter()
            .find(|entry| entry.id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(find(&composing.id).status, InvitationStatus::Interrupted);
    let unknown = find(&sending.id);
    assert_eq!(unknown.status, InvitationStatus::Unknown);
    assert_eq!(unknown.attempts[0].result, Some(AttemptResult::Unknown));
    assert_eq!(unknown.attempts[0].finished_at_ms, None, "不伪造完成时间");
    let interrupted = find(&judging.id);
    assert_eq!(interrupted.status, InvitationStatus::Pending);
    assert_eq!(
        interrupted.judgements[0].outcome,
        Some(JudgementOutcome::Interrupted)
    );
    assert_eq!(
        admin.begin_judgement(&judging.id, "msg-9", 300).err(),
        Some(OutreachError::Conflict),
        "中断的判断不重放"
    );
    admin.begin_judgement(&judging.id, "msg-10", 300).unwrap();
    assert_eq!(
        admin.claim(&sending.id, 200, passive("msg-2")).err(),
        Some(OutreachError::Conflict),
        "结果未知的邀请不重发"
    );
    assert!(
        admin
            .begin(
                "owner-a",
                "goal-1",
                milestone(),
                facts(),
                "composer:v1",
                300
            )
            .unwrap()
            .is_none(),
        "中断的撰写不重放"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn cancellation_quiet_preference_and_failed_commits() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await.unwrap();
    let first = pending(&admin, "owner-a", "goal-1", 100);
    let second = pending(&admin, "owner-a", "goal-2", 100);
    invite(&admin, &second.id, "msg-1", 140);
    admin.claim(&second.id, 150, passive("msg-1")).unwrap();
    assert_eq!(
        admin
            .cancel(&second.id, 160, CancelReason::GoalClosed)
            .err(),
        Some(OutreachError::Conflict),
        "正在发送的邀请不能取消"
    );
    let cancelled = admin
        .cancel(&first.id, 160, CancelReason::GoalClosed)
        .unwrap();
    assert_eq!(
        cancelled.status,
        InvitationStatus::Cancelled(CancelReason::GoalClosed)
    );

    let quiet = admin.set_quiet("owner-a", true, 170).unwrap();
    assert!(quiet.quiet);
    assert!(admin.snapshot().unwrap().quiet("owner-a"));
    assert!(!admin.set_quiet("owner-a", false, 180).unwrap().quiet);

    // 提交失败后关闭句柄，不确认任何变化；重新打开只读到已确认的结果。
    store.fail_before.store(true, Ordering::SeqCst);
    assert_eq!(
        admin
            .record_delivery(&second.id, 190, AttemptResult::NotSent)
            .err(),
        Some(OutreachError::Storage)
    );
    assert_eq!(admin.snapshot().err(), Some(OutreachError::Unavailable));
    store.fail_before.store(false, Ordering::SeqCst);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(store.clone()).await.unwrap();
    let reopened = admin.snapshot().unwrap();
    let entry = reopened
        .invitations
        .iter()
        .find(|entry| entry.id == second.id)
        .unwrap();
    assert_eq!(entry.status, InvitationStatus::Unknown);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_inconsistent_ledgers_are_refused_and_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path());
    let (kernel, admin) = open(store.clone()).await.unwrap();
    let invitation = pending(&admin, "owner-a", "goal-1", 100);
    invite(&admin, &invitation.id, "msg-1", 110);
    admin.claim(&invitation.id, 120, passive("msg-1")).unwrap();
    admin
        .record_delivery(
            &invitation.id,
            130,
            AttemptResult::Sent {
                platform_message_id: Some("out-1".into()),
            },
        )
        .unwrap();
    pending(&admin, "owner-a", "goal-2", 140);
    kernel.stop_all().await.unwrap();
    let valid = store.saved();

    let mut cases: Vec<Vec<u8>> = vec![
        b"{\"format_version\":1,\"format_version\":1,\"invitations\":[],\"preferences\":[]}"
            .to_vec(),
        serde_json::to_vec(&json!({"format_version": 9, "invitations": [], "preferences": []}))
            .unwrap(),
    ];
    let mutate = |change: &dyn Fn(&mut Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    // 已送达却没有成功回执、送达时间不符、两条同时发送、主动尝试超过一次、ID 与目标不符。
    cases.push(mutate(&|value| {
        value["invitations"][0]["attempts"][0]["result"] = json!("not_sent")
    }));
    cases.push(mutate(&|value| {
        value["invitations"][0]["delivered_at_ms"] = json!(999)
    }));
    cases.push(mutate(&|value| {
        for index in 0..2 {
            value["invitations"][index]["status"] = json!("Delivering");
            value["invitations"][index]["delivered_at_ms"] = Value::Null;
            value["invitations"][index]["closed_at_ms"] = Value::Null;
            value["invitations"][index]["attempts"] =
                json!([{"channel": "proactive", "started_at_ms": 200, "finished_at_ms": null, "result": null}]);
        }
    }));
    cases.push(mutate(&|value| {
        value["invitations"][0]["attempts"] = json!([
            {"channel": "proactive", "started_at_ms": 110, "finished_at_ms": 111, "result": "not_sent"},
            {"channel": "proactive", "started_at_ms": 120, "finished_at_ms": 130,
             "result": {"sent": {"platform_message_id": "out-1"}}}])
    }));
    cases.push(mutate(&|value| {
        value["invitations"][1]["goal_id"] = json!("goal-x")
    }));
    // 被动附带没有对应的“邀请”判断，或判断结论被改成此刻不合适。
    cases.push(mutate(&|value| {
        value["invitations"][0]["judgements"] = json!([])
    }));
    cases.push(mutate(&|value| {
        value["invitations"][0]["judgements"][0]["outcome"] = json!({"verdict": "NotNow"})
    }));
    for bytes in cases {
        store.overwrite(&bytes);
        assert!(open(store.clone()).await.is_err());
        assert_eq!(store.raw().unwrap(), bytes, "原字节保留");
    }
}
