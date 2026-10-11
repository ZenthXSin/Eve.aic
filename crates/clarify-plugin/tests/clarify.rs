use eve_clarify_api::*;
use eve_clarify_plugin::{
    COMPOSER_VERSION, ClarifyController, ClarifyPlugin, ModelClarificationComposer,
};
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_llm_api::{
    LlmError, LlmFuture, LlmModelResolver, LlmProvider, ModelRequest, ModelResponse, ModelSelection,
};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

struct Store {
    inner: FileStateStore,
    fail: AtomicBool,
}
impl StateStore for Store {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)
    }
}

struct World {
    _directory: tempfile::TempDir,
    state: Arc<Store>,
}
impl World {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let state = Arc::new(Store {
            inner: FileStateStore::open(directory.path().join("state")).unwrap(),
            fail: AtomicBool::new(false),
        });
        Self {
            _directory: directory,
            state,
        }
    }
    async fn open(&self) -> PluginResult<(Kernel, ClarifyController)> {
        let kernel = Kernel::with_services(KernelServices {
            state: self.state.clone(),
            ..KernelServices::default()
        });
        let plugin = ClarifyPlugin::new()?;
        let controller = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start_all().await?;
        Ok((kernel, controller))
    }
    fn raw(&self) -> Option<Vec<u8>> {
        self.state
            .get(
                &PluginId::new(CLARIFY_PLUGIN_ID).unwrap(),
                CLARIFY_STATE_KEY,
            )
            .unwrap()
    }
    fn write(&self, bytes: Vec<u8>) {
        self.state
            .inner
            .set(
                &PluginId::new(CLARIFY_PLUGIN_ID).unwrap(),
                CLARIFY_STATE_KEY.into(),
                bytes,
            )
            .unwrap();
    }
}

fn facts() -> Vec<Fact> {
    vec![
        Fact {
            kind: FactKind::UserQuote,
            text: "我喜欢这个游戏的模组".into(),
        },
        Fact {
            kind: FactKind::Attempt,
            text: "做了 3 次最小产物，实际运行都没有验证通过".into(),
        },
        Fact {
            kind: FactKind::Capability,
            text: "运行环境：只含数据文件的模组".into(),
        },
    ]
}

fn stall() -> Stall {
    Stall {
        kind: StallKind::PracticeUnverified,
        run_id: "practice-1".into(),
    }
}

async fn begin(admin: &ClarifyController, goal: &str, revision: u64, at: u64) -> Clarification {
    admin
        .begin(
            "user-1",
            goal,
            revision,
            stall(),
            facts(),
            COMPOSER_VERSION,
            at,
        )
        .unwrap()
        .unwrap()
}

fn answer(message: &str, at: u64) -> Answer {
    Answer {
        message_id: message.into(),
        evidence_id: format!("evidence-{message}"),
        at_ms: at,
        text: "想做一个会发光的墙".into(),
    }
}

#[tokio::test]
async fn one_question_per_goal_revision_is_judged_per_message_delivered_and_answered() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let request = begin(&admin, "goal-1", 2, 1_000).await;
    assert_eq!(request.id, clarification_id("goal-1", 2));
    assert_eq!(request.status, ClarifyStatus::Composing);
    // 同一目标修订只请求一次；新修订可以再请求。
    assert!(
        admin
            .begin(
                "user-1",
                "goal-1",
                2,
                stall(),
                facts(),
                COMPOSER_VERSION,
                1_001
            )
            .unwrap()
            .is_none()
    );
    assert!(
        admin
            .begin(
                "user-1",
                "goal-1",
                2,
                stall(),
                vec![],
                COMPOSER_VERSION,
                1_001
            )
            .is_err(),
        "事实不合规"
    );
    assert_eq!(
        admin
            .record_composition(&request.id, 1_100, Ok("没有问号。".into()))
            .unwrap_err(),
        ClarifyError::InvalidInput
    );
    let pending = admin
        .record_composition(
            &request.id,
            1_100,
            Ok("我试了三次都没加载成功，你想要的方块有什么效果？".into()),
        )
        .unwrap();
    assert_eq!(pending.status, ClarifyStatus::Pending);
    assert_eq!(
        admin.claim(&request.id, "m1", 1_150).unwrap_err(),
        ClarifyError::InvalidTransition,
        "须先判断"
    );

    admin.begin_judgement(&request.id, "m1", 1_200).unwrap();
    assert!(
        admin.begin_judgement(&request.id, "m2", 1_201).is_err(),
        "一次只判断一条消息"
    );
    admin
        .record_judgement(&request.id, "m1", 1_210, Ok(false))
        .unwrap();
    assert!(
        admin.begin_judgement(&request.id, "m1", 1_220).is_err(),
        "同一消息只判断一次"
    );
    assert!(
        admin.claim(&request.id, "m1", 1_230).is_err(),
        "不合适不附带"
    );
    admin.begin_judgement(&request.id, "m2", 1_300).unwrap();
    admin
        .record_judgement(&request.id, "m2", 1_310, Ok(true))
        .unwrap();
    let delivering = admin.claim(&request.id, "m2", 1_320).unwrap();
    assert_eq!(delivering.status, ClarifyStatus::Delivering);
    assert!(
        admin
            .cancel(&request.id, 1_325, CancelReason::GoalChanged)
            .is_err(),
        "发送中不能取消"
    );
    // 没有写出：回到等待，下一条消息再判断。
    let back = admin
        .record_delivery(&request.id, 1_330, DeliveryResult::NotSent)
        .unwrap();
    assert_eq!(back.status, ClarifyStatus::Pending);
    assert!(
        admin
            .record_delivery(&request.id, 1_331, DeliveryResult::Unknown)
            .is_err()
    );
    admin.begin_judgement(&request.id, "m3", 1_400).unwrap();
    admin
        .record_judgement(&request.id, "m3", 1_410, Ok(true))
        .unwrap();
    admin.claim(&request.id, "m3", 1_420).unwrap();
    let delivered = admin
        .record_delivery(
            &request.id,
            1_430,
            DeliveryResult::Sent {
                platform_message_id: Some("platform-1".into()),
            },
        )
        .unwrap();
    assert_eq!(
        (delivered.status, delivered.delivered_at_ms),
        (ClarifyStatus::Delivered, Some(1_430))
    );
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.listening().count(), 1);
    // 冷却期内不再投递同一用户的其他问题。
    let other = begin(&admin, "goal-2", 1, 1_440).await;
    admin
        .record_composition(&other.id, 1_450, Ok("还想补充什么？".into()))
        .unwrap();
    let policy = ClarifyPolicy {
        cooldown_ms: 10_000,
    };
    assert!(
        admin
            .snapshot()
            .unwrap()
            .due_for("user-1", 2_000, &policy)
            .is_none()
    );
    assert_eq!(
        admin
            .snapshot()
            .unwrap()
            .due_for("user-1", 11_430, &policy)
            .unwrap()
            .id,
        other.id
    );

    // 回答须在送达之后、不是附带问题的那条消息。
    assert!(
        admin
            .record_answer(&request.id, answer("m4", 1_000))
            .is_err()
    );
    assert!(
        admin
            .record_answer(&request.id, answer("m3", 1_500))
            .is_err()
    );
    assert!(admin.expire(&request.id, 1_500).is_err(), "回答窗口未过");
    let answered = admin
        .record_answer(&request.id, answer("m4", 1_500))
        .unwrap();
    assert_eq!(answered.status, ClarifyStatus::Answered);
    assert_eq!(answered.answer.unwrap().message_id, "m4");

    // 要求安静后不再投递；取消只针对尚未送达的问题。
    admin.set_quiet("user-1", true, 1_600).unwrap();
    assert!(
        admin
            .snapshot()
            .unwrap()
            .due_for("user-1", 99_999, &policy)
            .is_none()
    );
    let cancelled = admin
        .cancel(&other.id, 1_700, CancelReason::GoalChanged)
        .unwrap();
    assert_eq!(
        cancelled.status,
        ClarifyStatus::Cancelled(CancelReason::GoalChanged)
    );
    kernel.stop_all().await.unwrap();

    // 重启后记录不变。
    let (kernel, admin) = world.open().await.unwrap();
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.requests.len(), 2);
    assert!(snapshot.quiet("user-1"));
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn interrupted_work_is_not_replayed_and_unanswered_questions_expire() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let composing = begin(&admin, "goal-a", 1, 1_000).await;
    let judging = begin(&admin, "goal-b", 1, 1_000).await;
    admin
        .record_composition(&judging.id, 1_100, Ok("想要什么效果？".into()))
        .unwrap();
    admin.begin_judgement(&judging.id, "m1", 1_200).unwrap();
    let sending = begin(&admin, "goal-c", 1, 1_000).await;
    admin
        .record_composition(&sending.id, 1_100, Ok("想要什么效果？".into()))
        .unwrap();
    admin.begin_judgement(&sending.id, "m2", 1_200).unwrap();
    admin
        .record_judgement(&sending.id, "m2", 1_210, Ok(true))
        .unwrap();
    admin.claim(&sending.id, "m2", 1_220).unwrap();
    let delivered = begin(&admin, "goal-d", 1, 1_000).await;
    admin
        .record_composition(&delivered.id, 1_100, Ok("想要什么效果？".into()))
        .unwrap();
    admin.begin_judgement(&delivered.id, "m3", 1_200).unwrap();
    admin
        .record_judgement(&delivered.id, "m3", 1_210, Ok(true))
        .unwrap();
    admin.claim(&delivered.id, "m3", 1_220).unwrap();
    admin
        .record_delivery(
            &delivered.id,
            1_230,
            DeliveryResult::Sent {
                platform_message_id: None,
            },
        )
        .unwrap();
    kernel.stop_all().await.unwrap();

    let (kernel, admin) = world.open().await.unwrap();
    let snapshot = admin.snapshot().unwrap();
    let status = |id: &str| {
        snapshot
            .requests
            .iter()
            .find(|request| request.id == id)
            .unwrap()
            .clone()
    };
    assert_eq!(status(&composing.id).status, ClarifyStatus::Interrupted);
    let judging = status(&judging.id);
    assert_eq!(judging.status, ClarifyStatus::Pending);
    assert_eq!(
        judging.judgements[0].outcome,
        Some(JudgeOutcome::Interrupted)
    );
    assert_eq!(judging.judgements[0].finished_at_ms, None, "不伪造完成时间");
    let sending = status(&sending.id);
    assert_eq!(sending.status, ClarifyStatus::Unknown);
    assert_eq!(sending.attempts[0].result, Some(DeliveryResult::Unknown));
    // 中断的撰写不能再记录结果，结果未知的投递不能再取消或重发。
    assert!(
        admin
            .record_composition(&composing.id, 2_000, Ok("问？".into()))
            .is_err()
    );
    assert!(
        admin
            .cancel(&sending.id, 2_000, CancelReason::GoalClosed)
            .is_err()
    );
    // 回答窗口过后记为过期。
    assert!(
        admin
            .expire(&delivered.id, 1_230 + ANSWER_WINDOW_MS)
            .is_err()
    );
    let expired = admin
        .expire(&delivered.id, 1_231 + ANSWER_WINDOW_MS)
        .unwrap();
    assert_eq!(expired.status, ClarifyStatus::Expired);
    assert!(
        admin
            .record_answer(&delivered.id, answer("m9", 1_300))
            .is_err()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn judgements_are_bounded_and_storage_failure_closes_the_instance() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let request = begin(&admin, "goal-1", 1, 1_000).await;
    admin
        .record_composition(&request.id, 1_100, Ok("想要什么？".into()))
        .unwrap();
    for index in 0..MAX_JUDGEMENTS as u64 {
        let message = format!("m{index}");
        admin
            .begin_judgement(&request.id, &message, 2_000 + index)
            .unwrap();
        admin
            .record_judgement(&request.id, &message, 2_000 + index, Ok(false))
            .unwrap();
    }
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(
        snapshot.requests[0].status,
        ClarifyStatus::Failed(ClarifyFailure::Exhausted),
        "判断次数用尽不再等待"
    );
    let second = begin(&admin, "goal-2", 1, 3_000).await;
    world.state.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        admin
            .record_composition(&second.id, 3_100, Ok("想要什么？".into()))
            .unwrap_err(),
        ClarifyError::Storage
    );
    world.state.fail.store(false, Ordering::SeqCst);
    assert_eq!(admin.snapshot().unwrap_err(), ClarifyError::Unavailable);
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = world.open().await.unwrap();
    // 重新打开后按已提交的记录核对：撰写未提交，记为中断，不重放。
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.requests[1].status, ClarifyStatus::Interrupted);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn corrupt_or_unknown_ledgers_are_refused_and_preserved() {
    let world = World::new();
    let (kernel, admin) = world.open().await.unwrap();
    let request = begin(&admin, "goal-1", 1, 1_000).await;
    admin
        .record_composition(&request.id, 1_100, Ok("想要什么效果？".into()))
        .unwrap();
    admin.begin_judgement(&request.id, "m1", 1_200).unwrap();
    admin
        .record_judgement(&request.id, "m1", 1_210, Ok(true))
        .unwrap();
    admin.claim(&request.id, "m1", 1_220).unwrap();
    admin
        .record_delivery(
            &request.id,
            1_230,
            DeliveryResult::Sent {
                platform_message_id: None,
            },
        )
        .unwrap();
    admin
        .record_answer(&request.id, answer("m2", 1_300))
        .unwrap();
    admin.set_quiet("user-1", true, 1_400).unwrap();
    kernel.stop_all().await.unwrap();
    let valid: serde_json::Value = serde_json::from_slice(&world.raw().unwrap()).unwrap();
    let mutate = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value = valid.clone();
        change(&mut value);
        serde_json::to_vec(&value).unwrap()
    };
    let cases = vec![
        b"{\"format_version\":1,\"format_version\":1,\"requests\":[],\"preferences\":[]}".to_vec(),
        serde_json::to_vec(
            &serde_json::json!({"format_version": 2, "requests": [], "preferences": []}),
        )
        .unwrap(),
        mutate(&|value| value["requests"][0]["id"] = "clarify-other".into()),
        mutate(&|value| value["requests"][0]["question"] = "没有问号".into()),
        mutate(&|value| value["requests"][0]["answer"]["message_id"] = "m1".into()),
        mutate(&|value| value["requests"][0]["status"] = "Delivered".into()),
        mutate(&|value| value["requests"][0]["judgements"][0]["outcome"] = "not_now".into()),
        mutate(&|value| value["requests"][0]["facts"] = serde_json::json!([])),
        mutate(&|value| {
            let first = value["requests"][0].clone();
            value["requests"].as_array_mut().unwrap().push(first)
        }),
        mutate(&|value| {
            let first = value["preferences"][0].clone();
            value["preferences"].as_array_mut().unwrap().push(first)
        }),
        mutate(&|value| value["extra"] = 1.into()),
    ];
    for case in cases {
        world.write(case.clone());
        assert!(
            world.open().await.is_err(),
            "{}",
            String::from_utf8_lossy(&case)
        );
        assert_eq!(world.raw().unwrap(), case, "损坏的账本原样保留");
    }
    world.write(serde_json::to_vec(&valid).unwrap());
    let (kernel, admin) = world.open().await.unwrap();
    assert_eq!(
        admin.snapshot().unwrap().requests[0].status,
        ClarifyStatus::Answered
    );
    kernel.stop_all().await.unwrap();
}

struct Scripted {
    replies: Mutex<Vec<Result<ModelResponse, LlmError>>>,
    seen: Mutex<Vec<ModelRequest>>,
}
impl LlmProvider for Scripted {
    fn complete(&self, request: ModelRequest) -> LlmFuture<'_, ModelResponse> {
        self.seen.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().remove(0);
        Box::pin(async move { reply })
    }
}
struct Fixed(Arc<Scripted>);
impl LlmModelResolver for Fixed {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        Ok(ModelSelection {
            provider: self.0.clone(),
            provider_timeout: Duration::from_secs(5),
        })
    }
}

fn compose_request() -> ComposeRequest {
    ComposeRequest {
        request_id: clarification_id("goal-1", 1),
        composer_version: COMPOSER_VERSION.into(),
        stall: StallKind::PracticeUnverified,
        facts: facts(),
    }
}

#[tokio::test]
async fn the_composer_makes_one_tool_free_request_and_rejects_non_questions() {
    let final_text = |text: &str| Ok(ModelResponse::Final { text: text.into() });
    let provider = Arc::new(Scripted {
        replies: Mutex::new(vec![
            final_text(r#"{"question":"我试了三次都没加载成功，你想要的方块有什么效果？"}"#),
            final_text(r#"{"question":"我会继续努力。"}"#),
            final_text(r#"{"question":"想要什么？","question":"重复键？"}"#),
            final_text(r#"{"question":"想要什么？","extra":1}"#),
            Err(LlmError::ProviderTimeout),
        ]),
        seen: Mutex::new(vec![]),
    });
    let composer = ModelClarificationComposer::new(Arc::new(Fixed(provider.clone())));
    assert_eq!(composer.version(), COMPOSER_VERSION);
    assert_eq!(
        composer.compose(compose_request()).await.unwrap(),
        "我试了三次都没加载成功，你想要的方块有什么效果？"
    );
    for _ in 0..3 {
        assert_eq!(
            composer.compose(compose_request()).await.unwrap_err(),
            ClarifyError::Clarify(ClarifyFailure::InvalidOutput)
        );
    }
    assert_eq!(
        composer.compose(compose_request()).await.unwrap_err(),
        ClarifyError::Clarify(ClarifyFailure::Timeout)
    );
    let mut wrong = compose_request();
    wrong.composer_version = "other".into();
    assert_eq!(
        composer.compose(wrong).await.unwrap_err(),
        ClarifyError::InvalidInput
    );
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 5, "版本不符时不请求");
    assert!(seen.iter().all(|request| request.tools.is_empty()));
    let body: serde_json::Value =
        serde_json::from_str(seen[0].messages.last().unwrap().text.as_deref().unwrap()).unwrap();
    assert_eq!(body["stall"], "practice_unverified");
    assert_eq!(body["facts"][0]["kind"], "UserQuote");
    let system = seen[0].messages[0].text.as_deref().unwrap();
    for domain in ["Mindustry", "模组", "游戏"] {
        assert!(!system.contains(domain), "规则不能按领域分支");
    }
}
