mod support;

use eve_interest_api::*;
use eve_interest_plugin::{InterestController, InterestPlugin};
use eve_kernel::{Kernel, KernelServices};
use eve_plugin_api::{PluginId, StateStore};
use serde_json::Value;
use std::sync::Arc;
use support::*;

fn owner() -> PluginId {
    PluginId::new(INTEREST_PLUGIN_ID).unwrap()
}

async fn open(state: Arc<dyn StateStore>) -> (Kernel, InterestController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = InterestPlugin::new().unwrap();
    let admin = plugin.controller();
    assert_eq!(
        admin.snapshot(&scope("s", "u")).err(),
        Some(InterestError::Unavailable)
    );
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}

fn stored(store: &RecordingStore) -> Value {
    serde_json::from_slice(&store.get(&owner(), INTEREST_STATE_KEY).unwrap().unwrap()).unwrap()
}

fn existing(id: &str, statements: Vec<StatementDraft>) -> InterestUpdateDraft {
    InterestUpdateDraft {
        target: InterestTarget::Existing { id: id.into() },
        statements,
        inferred_need: None,
    }
}

#[tokio::test]
async fn reserve_is_durable_before_observation_and_finish_keeps_quotes_and_marked_inference() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let alice = scope("session-a", "alice");
    let source = memory(alice.clone(), &[INTEREST]);
    let batch = reserve(&admin, &source, 100);
    assert_eq!(batch.evidence, source.evidence);
    assert!(batch.known_interests.is_empty());
    let saved = stored(&store);
    assert_eq!(saved["jobs"][0]["job"]["status"], "Running");
    assert_eq!(
        serde_json::from_value::<ObservationBatch>(saved["jobs"][0]["job"]["batch"].clone())
            .unwrap(),
        batch
    );
    let update = new_interest(&batch, "Mindustry 模组创作");
    let results = admin
        .finish(
            &batch,
            110,
            ObservationOutcome::Completed(vec![update.clone()]),
        )
        .unwrap();
    let [UpdateResult::Created { interest_id }] = results.as_slice() else {
        panic!("one interest must be created");
    };
    let snapshot = admin.snapshot(&alice).unwrap();
    assert_eq!(snapshot.jobs[0].status, JobStatus::Completed);
    assert_eq!(snapshot.jobs[0].finished_at_ms, Some(110));
    assert_eq!(snapshot.jobs[0].updates, vec![update.clone()]);
    let interest = &snapshot.interests[0];
    assert_eq!(&interest.id, interest_id);
    assert_eq!(interest.topic, "Mindustry 模组创作");
    assert_eq!(interest.status, InterestStatus::Active);
    assert_eq!(interest.revision, 1);
    assert_eq!(interest.created_at_ms, 110);
    assert_eq!(interest.statements.len(), 2);
    for statement in &interest.statements {
        assert_eq!(statement.evidence_id, "e-1");
        assert_eq!(statement.message_id, "message-1");
        assert_eq!(
            statement.observed_at_ms, 10,
            "保存原始交互时间，不是处理时间"
        );
        assert_eq!(
            statement.origin,
            StatementOrigin::Observation {
                batch_id: batch.id.clone()
            }
        );
        assert!(INTEREST.contains(&statement.quote));
    }
    assert_eq!(interest.inferences.len(), 1);
    assert_eq!(interest.inferences[0].text, "可能希望学习如何制作模组");
    assert_eq!(interest.inferences[0].interest_revision, 1);
    // 同一结局重放零写入并返回原结果；不同结局不能覆盖。
    let writes = store.writes();
    assert_eq!(
        admin
            .finish(&batch, 120, ObservationOutcome::Completed(vec![update]))
            .unwrap(),
        results
    );
    assert_eq!(
        admin.finish(
            &batch,
            120,
            ObservationOutcome::Failed(ObservationFailure::Provider)
        ),
        Err(InterestError::Conflict)
    );
    assert_eq!(store.writes(), writes);
    // 其他作用域看不到这条兴趣。
    assert!(
        admin
            .snapshot(&scope("session-b", "bob"))
            .unwrap()
            .interests
            .is_empty()
    );
    let debug = format!("{batch:?} {snapshot:?} {interest:?}");
    for private in [INTEREST, "alice", "Mindustry", "可能希望"] {
        assert!(!debug.contains(private));
    }
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn quotes_must_be_user_text_and_targets_must_be_known_or_new_interests() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let alice = scope("session-a", "alice");
    let batch = reserve(&admin, &memory(alice.clone(), &[INTEREST]), 100);
    let valid = new_interest(&batch, "模组创作");
    let mut cases = Vec::new();
    // 引用助手回复、改写原话、外部证据、无兴趣陈述的新主题都不能落地。
    let mut assistant = valid.clone();
    assistant.statements[0].quote = "Mindustry 模组很好玩".into();
    cases.push(vec![assistant]);
    let mut paraphrase = valid.clone();
    paraphrase.statements[0].quote = "我很喜欢 Mindustry 模组".into();
    cases.push(vec![paraphrase]);
    let mut foreign = valid.clone();
    foreign.statements[0].evidence_id = "foreign-evidence".into();
    cases.push(vec![foreign]);
    let mut difficulty_only = valid.clone();
    difficulty_only.statements.remove(0);
    cases.push(vec![difficulty_only]);
    let mut withdrawal_new = valid.clone();
    withdrawal_new.statements = vec![statement(
        StatementKind::Withdrawal,
        "不知道怎么创作",
        "e-1",
    )];
    withdrawal_new.inferred_need = None;
    cases.push(vec![withdrawal_new]);
    cases.push(vec![existing(
        "interest-unknown",
        vec![statement(
            StatementKind::Experience,
            "不知道怎么创作",
            "e-1",
        )],
    )]);
    let mut duplicate_topic = valid.clone();
    duplicate_topic.target = InterestTarget::New {
        topic: " 模组 创作".into(),
    };
    cases.push(vec![valid.clone(), duplicate_topic]);
    cases.push(vec![valid.clone(); MAX_UPDATES + 1]);
    let mut repeated = valid.clone();
    repeated.statements.push(repeated.statements[0].clone());
    cases.push(vec![repeated]);
    let mut empty_need = valid.clone();
    empty_need.inferred_need = Some(" ".into());
    cases.push(vec![empty_need]);
    let writes = store.writes();
    for updates in cases {
        assert_eq!(
            validate_updates(&batch, &updates),
            Err(InterestError::InvalidInput)
        );
        assert_eq!(
            admin.finish(&batch, 110, ObservationOutcome::Completed(updates)),
            Err(InterestError::InvalidInput)
        );
    }
    assert_eq!(store.writes(), writes, "不合规输出不写账本");
    assert_eq!(
        admin.snapshot(&alice).unwrap().jobs[0].status,
        JobStatus::Running
    );
    // 引用忽略空白差异，但仍须是同一段原文。
    let mut spaced = valid;
    spaced.statements[1].quote = "不知道 怎么\n创作".into();
    admin
        .finish(&batch, 110, ObservationOutcome::Completed(vec![spaced]))
        .unwrap();
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn follow_ups_extend_one_interest_and_withdrawal_stops_later_updates() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let alice = scope("session-a", "alice");
    let texts = [
        INTEREST,
        "我之前只改过一点 JSON 配置。",
        "我还是想做 Mindustry 模组创作。",
        "算了，我现在对 Mindustry 模组没兴趣了。",
        "我会写一点 Java。",
    ];
    let first = reserve(&admin, &memory(alice.clone(), &texts[..1]), 100);
    admin
        .finish(
            &first,
            101,
            ObservationOutcome::Completed(vec![new_interest(&first, "Mindustry 模组创作")]),
        )
        .unwrap();
    let id = admin.snapshot(&alice).unwrap().interests[0].id.clone();
    let second = reserve(&admin, &memory(alice.clone(), &texts[..2]), 200);
    assert_eq!(second.evidence.len(), 1, "已观察的交互不再进入新批次");
    assert_eq!(second.known_interests[0].id, id);
    let results = admin
        .finish(
            &second,
            201,
            ObservationOutcome::Completed(vec![existing(
                &id,
                vec![statement(
                    StatementKind::Experience,
                    "我之前只改过一点 JSON 配置",
                    "e-2",
                )],
            )]),
        )
        .unwrap();
    assert_eq!(
        results,
        vec![UpdateResult::Updated {
            interest_id: id.clone(),
            revision: 2
        }]
    );
    // 同名新主题并入当前兴趣，不另建记录。
    let third = reserve(&admin, &memory(alice.clone(), &texts[..3]), 300);
    let mut same_topic = InterestUpdateDraft {
        target: InterestTarget::New {
            topic: "mindustry模组创作".into(),
        },
        statements: vec![statement(
            StatementKind::Interest,
            "我还是想做 Mindustry 模组创作",
            "e-3",
        )],
        inferred_need: Some("可能需要一个入门示例".into()),
    };
    assert_eq!(
        admin
            .finish(
                &third,
                301,
                ObservationOutcome::Completed(vec![same_topic.clone()])
            )
            .unwrap(),
        vec![UpdateResult::Updated {
            interest_id: id.clone(),
            revision: 3
        }]
    );
    // 批次准备后用户先用命令撤回，旧批次的补充不能复活兴趣。
    let fourth = reserve(&admin, &memory(alice.clone(), &texts[..4]), 400);
    let withdrawn = admin
        .withdraw(
            &alice,
            &id,
            WithdrawalRequest {
                evidence_id: "command-1".into(),
                message_id: "command-message-1".into(),
                text: format!("/forget-interest {id}"),
                at_ms: 401,
            },
        )
        .unwrap();
    assert_eq!(withdrawn.status, InterestStatus::Withdrawn);
    assert_eq!(withdrawn.revision, 4);
    assert_eq!(
        withdrawn.statements.last().unwrap().origin,
        StatementOrigin::UserCommand
    );
    same_topic.target = InterestTarget::Existing { id: id.clone() };
    same_topic.statements = vec![statement(
        StatementKind::Withdrawal,
        "我现在对 Mindustry 模组没兴趣了",
        "e-4",
    )];
    same_topic.inferred_need = None;
    assert_eq!(
        admin
            .finish(
                &fourth,
                402,
                ObservationOutcome::Completed(vec![same_topic])
            )
            .unwrap(),
        vec![UpdateResult::Rejected {
            reason: RejectReason::NotActive
        }]
    );
    // 撤回后的兴趣不再交给观察器关联；再次表达只能形成新兴趣。
    let fifth = reserve(&admin, &memory(alice.clone(), &texts), 500);
    assert!(fifth.known_interests.is_empty());
    let interests = admin.snapshot(&alice).unwrap().interests;
    assert_eq!(interests.len(), 1);
    assert_eq!(interests[0].revision, 4);
    assert_eq!(interests[0].inferences.len(), 2);
    assert_eq!(interests[0].inferences[1].interest_revision, 3);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn restart_interrupts_running_batches_and_consumed_evidence_is_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let alice = scope("session-a", "alice");
    let source = memory(alice.clone(), &[INTEREST]);
    {
        let (kernel, admin) = open(store.clone()).await;
        let _running = reserve(&admin, &source, 100);
        kernel.stop_all().await.unwrap();
    }
    let (kernel, admin) = open(store.clone()).await;
    let jobs = admin.snapshot(&alice).unwrap().jobs;
    assert_eq!(jobs[0].status, JobStatus::Interrupted);
    assert_eq!(jobs[0].finished_at_ms, None, "不伪造完成时间");
    assert_eq!(stored(&store)["jobs"][0]["job"]["status"], "Interrupted");
    assert_eq!(
        admin
            .reserve(&source, 200, "observer-v1", &options())
            .unwrap(),
        None,
        "中断批次的证据已消费"
    );
    let two = memory(alice.clone(), &[INTEREST, "我喜欢画水彩。"]);
    let batch = reserve(&admin, &two, 300);
    admin
        .finish(
            &batch,
            301,
            ObservationOutcome::Failed(ObservationFailure::Timeout),
        )
        .unwrap();
    assert_eq!(
        admin.reserve(&two, 400, "observer-v1", &options()).unwrap(),
        None
    );
    // 冷却与时钟回退都不准备新批次。
    let three = memory(
        alice.clone(),
        &[INTEREST, "我喜欢画水彩。", "我也喜欢下棋。"],
    );
    let slow = ObservationOptions {
        cooldown_ms: 1_000,
        ..ObservationOptions::default()
    };
    assert_eq!(
        admin.reserve(&three, 500, "observer-v1", &slow).unwrap(),
        None
    );
    assert_eq!(
        admin
            .reserve(&three, 200, "observer-v1", &options())
            .unwrap(),
        None
    );
    // 记忆回退或已消费证据被改写都视为冲突，不把旧交互当成新输入。
    let mut regressed = three.clone();
    regressed.revision = 1;
    assert_eq!(
        admin.reserve(&regressed, 1_400, "observer-v1", &options()),
        Err(InterestError::Conflict)
    );
    let mut rewritten = three.clone();
    rewritten.evidence[0].at_ms += 1;
    assert_eq!(
        admin.reserve(&rewritten, 1_400, "observer-v1", &options()),
        Err(InterestError::Conflict)
    );
    assert_eq!(reserve(&admin, &three, 1_400).evidence.len(), 1);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn withdrawal_always_fits_and_repeated_commands_write_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let alice = scope("session-a", "alice");
    let mut texts: Vec<String> = vec![INTEREST.into()];
    let first = reserve(&admin, &memory(alice.clone(), &[INTEREST]), 100);
    admin
        .finish(
            &first,
            100,
            ObservationOutcome::Completed(vec![new_interest(&first, "模组创作")]),
        )
        .unwrap();
    let id = admin.snapshot(&alice).unwrap().interests[0].id.clone();
    // 普通陈述最多 15 条；第 16 个位置保留给撤回。
    let mut last = vec![];
    for round in 0..4_u64 {
        texts.push(format!(
            "第一点{round} 第二点{round} 第三点{round} 第四点{round}"
        ));
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let batch = reserve(&admin, &memory(alice.clone(), &refs), 200 + round);
        let evidence = batch.evidence[0].id.clone();
        let statements = (1..=4)
            .map(|index| {
                let quote = ["第一点", "第二点", "第三点", "第四点"][index - 1];
                statement(
                    StatementKind::Experience,
                    &format!("{quote}{round}"),
                    &evidence,
                )
            })
            .collect::<Vec<_>>();
        last = admin
            .finish(
                &batch,
                300 + round,
                ObservationOutcome::Completed(vec![existing(&id, statements)]),
            )
            .unwrap();
    }
    assert_eq!(
        last,
        vec![UpdateResult::Rejected {
            reason: RejectReason::StatementLimit
        }]
    );
    let saved = admin.snapshot(&alice).unwrap().interests[0].clone();
    assert_eq!(saved.statements.len(), 14);
    let request = WithdrawalRequest {
        evidence_id: "command-1".into(),
        message_id: "command-message-1".into(),
        text: format!("/forget-interest {id}"),
        at_ms: 500,
    };
    let withdrawn = admin.withdraw(&alice, &id, request.clone()).unwrap();
    assert_eq!(withdrawn.statements.len(), 15);
    let writes = store.writes();
    assert_eq!(
        admin.withdraw(&alice, &id, request.clone()).unwrap(),
        withdrawn
    );
    let mut later = request.clone();
    later.evidence_id = "command-2".into();
    assert_eq!(admin.withdraw(&alice, &id, later).unwrap(), withdrawn);
    assert_eq!(store.writes(), writes, "重复撤回零写入");
    assert_eq!(
        admin.withdraw(&scope("session-b", "bob"), &id, request.clone()),
        Err(InterestError::NotFound),
        "不能撤回其他会话的兴趣"
    );
    let mut long = request;
    long.text = "撤".repeat(MAX_QUOTE_BYTES);
    assert_eq!(
        admin.withdraw(&alice, &id, long),
        Err(InterestError::InvalidInput)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn interest_capacity_rejects_new_topics_without_evicting_existing_records() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let alice = scope("session-a", "alice");
    let mut texts: Vec<String> = vec![];
    let mut created = 0;
    let mut rejected = 0;
    for round in 0..22_u64 {
        texts.push(format!(
            "我喜欢主题甲{round}，我喜欢主题乙{round}，我喜欢主题丙{round}。"
        ));
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let batch = reserve(&admin, &memory(alice.clone(), &refs), 100 + round);
        let evidence = batch.evidence[0].id.clone();
        let updates = ["甲", "乙", "丙"]
            .iter()
            .map(|name| InterestUpdateDraft {
                target: InterestTarget::New {
                    topic: format!("主题{name}{round}"),
                },
                statements: vec![statement(
                    StatementKind::Interest,
                    &format!("我喜欢主题{name}{round}"),
                    &evidence,
                )],
                inferred_need: None,
            })
            .collect();
        for result in admin
            .finish(&batch, 200 + round, ObservationOutcome::Completed(updates))
            .unwrap()
        {
            match result {
                UpdateResult::Created { .. } => created += 1,
                UpdateResult::Rejected {
                    reason: RejectReason::InterestLimit,
                } => rejected += 1,
                other => panic!("unexpected result {other:?}"),
            }
        }
    }
    assert_eq!((created, rejected), (MAX_INTERESTS, 66 - MAX_INTERESTS));
    let interests = admin.snapshot(&alice).unwrap().interests;
    assert_eq!(interests.len(), MAX_INTERESTS);
    assert_eq!(interests[0].topic, "主题甲0", "达到容量不淘汰旧兴趣");
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn uncertain_commit_closes_handles_and_corrupt_state_is_preserved() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let alice = scope("session-a", "alice");
    {
        let (kernel, admin) = open(store.clone()).await;
        let batch = reserve(&admin, &memory(alice.clone(), &[INTEREST]), 100);
        store.fail_after(true);
        assert_eq!(
            admin.finish(
                &batch,
                110,
                ObservationOutcome::Completed(vec![new_interest(&batch, "模组创作")])
            ),
            Err(InterestError::Storage)
        );
        store.fail_after(false);
        assert_eq!(admin.scopes(), Err(InterestError::Unavailable));
        let _ = kernel.stop_all().await;
    }
    {
        // 提交其实已经落盘；重新打开后读到真实结局。
        let (kernel, admin) = open(store.clone()).await;
        assert_eq!(admin.snapshot(&alice).unwrap().interests.len(), 1);
        kernel.stop_all().await.unwrap();
    }
    let original = store.get(&owner(), INTEREST_STATE_KEY).unwrap().unwrap();
    let text = String::from_utf8(original.clone()).unwrap();
    let tampered = [
        // 改写引用后无法在原始交互中找到。
        text.replace("不知道怎么创作\"", "我是专家\""),
        // 重复键不能被静默覆盖。
        text.replacen(
            "{\"format_version\":1,",
            "{\"format_version\":1,\"format_version\":1,",
            1,
        ),
        text.replacen("\"format_version\":1", "\"format_version\":9", 1),
    ];
    for (index, bytes) in tampered.iter().enumerate() {
        assert_ne!(bytes.as_bytes(), original.as_slice());
        store
            .set(
                &owner(),
                INTEREST_STATE_KEY.into(),
                bytes.clone().into_bytes(),
            )
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(InterestPlugin::new().unwrap()))
            .unwrap();
        let error = kernel.start_all().await.unwrap_err().to_string();
        let expected = if index == 2 { "不支持" } else { "损坏" };
        assert!(error.contains(expected), "{error}");
        assert_eq!(
            store.get(&owner(), INTEREST_STATE_KEY).unwrap().unwrap(),
            bytes.as_bytes(),
            "损坏状态保留原字节"
        );
    }
}
