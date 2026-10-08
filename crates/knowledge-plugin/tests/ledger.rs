mod support;

use eve_knowledge_api::*;
use eve_knowledge_plugin::Researcher;
use serde_json::{Value, json};
use std::sync::Arc;
use support::*;

fn researcher(
    admin: &eve_knowledge_plugin::KnowledgeController,
    fetcher: Arc<FakeFetcher>,
    selector: Arc<FakeSelector>,
    extractor: Arc<FakeExtractor>,
) -> Researcher {
    Researcher::new(Arc::new(admin.clone()), fetcher, selector, extractor)
}

fn stage(saved: &Option<Value>) -> (Value, Value) {
    let run = &saved.as_ref().expect("state saved before external effect")["runs"][0];
    (run["status"].clone(), run["stage"].clone())
}

#[tokio::test]
async fn every_stage_is_saved_before_its_external_effect_and_completion_keeps_sources_apart() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let fetcher = FakeFetcher::new(Some(store.clone()));
    let mut index = index_page();
    // 入口页面上的站外链接已由抓取器过滤；这里额外提供一个指向入口自身的链接。
    index.links.push(PageLink {
        url: SEED.into(),
        text: "Home".into(),
    });
    fetcher.serve(SEED, Ok(index));
    fetcher.serve(START, Ok(page(START, START_TEXT, &[])));
    let selector = FakeSelector::new(Some(store.clone()), vec![0]);
    let extractor = FakeExtractor::new(Some(store.clone()), |_| {
        Ok(serde_json::to_value(output_json()).unwrap())
    });
    let research = researcher(&admin, fetcher.clone(), selector.clone(), extractor.clone());
    let run = admin
        .begin(topic("goal-a", 1), &policy(), &research.version(), 100)
        .unwrap()
        .unwrap();
    assert_eq!(run.researcher_version, "fake-selector:v1+fake-extractor:v1");
    let outcome = research.research(&run, || 120).await.unwrap();
    let finished = admin.finish(&run.id, 130, outcome).unwrap();

    let calls = fetcher.calls.lock().unwrap().clone();
    assert_eq!(fetcher_urls(&calls), [SEED, START]);
    assert_eq!(stage(&calls[0].1), (json!("Running"), json!("Discovering")));
    assert_eq!(stage(&calls[1].1), (json!("Running"), json!("Fetching")));
    let selection = selector.requests.lock().unwrap()[0].clone();
    assert_eq!(stage(&selection.1), (json!("Running"), json!("Selecting")));
    let urls: Vec<_> = selection
        .0
        .candidates
        .iter()
        .map(|c| c.url.as_str())
        .collect();
    assert_eq!(urls, [START, BLOCKS], "入口自身不作为候选");
    assert_eq!(selection.0.brief, topic("goal-a", 1).brief);
    let extraction = extractor.requests.lock().unwrap()[0].clone();
    assert_eq!(
        stage(&extraction.1),
        (json!("Running"), json!("Extracting"))
    );
    assert_eq!(extraction.0.documents.len(), 1);
    assert_eq!(extraction.0.documents[0].text, START_TEXT);
    assert_eq!(extraction.0.documents[0].fetched_at_ms, 120);

    assert_eq!(finished.status, RunStatus::Completed);
    assert_eq!(finished.finished_at_ms, Some(130));
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.documents.len(), 2);
    let [quoted, hypothesis] = snapshot.entries.as_slice() else {
        panic!("one quoted claim and one hypothesis");
    };
    assert_eq!(quoted.status, KnowledgeStatus::SourceQuoted);
    assert_eq!(quoted.kind, KnowledgeKind::Procedure);
    assert_eq!(quoted.version.as_deref(), Some("146"));
    assert_eq!(quoted.owner, "user-a");
    let source = quoted.source.as_ref().unwrap();
    assert_eq!(snapshot.document(&source.document_id).unwrap().url, START);
    assert_eq!(hypothesis.status, KnowledgeStatus::Unverified);
    assert!(hypothesis.source.is_none() && hypothesis.version.is_none());
    assert_eq!(
        finished.results,
        vec![
            EntryResult::Created {
                entry_id: quoted.id.clone()
            },
            EntryResult::Created {
                entry_id: hypothesis.id.clone()
            }
        ]
    );
    kernel.stop_all().await.unwrap();

    // 重新打开读到同一份已提交结果；同一修订不再准入。
    let (kernel, admin) = open(store.clone()).await;
    assert_eq!(admin.snapshot().unwrap(), snapshot);
    assert!(
        admin
            .begin(topic("goal-a", 1), &policy(), "v", 200)
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}

fn fetcher_urls(calls: &[(String, Option<Value>)]) -> Vec<&str> {
    calls.iter().map(|(url, _)| url.as_str()).collect()
}

fn output_json() -> Value {
    json!({
        "claims": [{
            "document_id": start_document(),
            "kind": "procedure",
            "statement": "每个模组根目录需要 mod.hjson",
            "quote": "Each mod needs a mod.hjson file in its root.",
            "version": "146"
        }],
        "hypotheses": [{"statement": "可能需要先建立 content 目录"}]
    })
}

#[tokio::test]
async fn new_revisions_research_again_dedupe_known_knowledge_and_stop_at_the_persisted_cap() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let mut first = None;
    for revision in 1..=MAX_RUNS_PER_GOAL as u64 {
        let run = admin
            .begin(topic("goal-a", revision), &policy(), "v", 100 + revision)
            .unwrap()
            .unwrap();
        // 已有研究在进行时不准入另一个目标。
        assert!(
            admin
                .begin(topic("goal-b", 1), &policy(), "v", 100)
                .unwrap()
                .is_none()
        );
        advance_to_extracting(&admin, &run);
        let finished = admin
            .finish(
                &run.id,
                200 + revision,
                ResearchOutcome::Completed(output()),
            )
            .unwrap();
        match &first {
            None => first = Some(finished.results.clone()),
            Some(created) => {
                let ids: Vec<_> = created
                    .iter()
                    .map(|result| match result {
                        EntryResult::Created { entry_id } => EntryResult::Duplicate {
                            entry_id: entry_id.clone(),
                        },
                        other => other.clone(),
                    })
                    .collect();
                assert_eq!(finished.results, ids, "同来源同引用与相同假设不重复保存");
            }
        }
    }
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.entries.len(), 2);
    assert_eq!(snapshot.documents.len(), 2, "同一内容只保存一次");
    assert_eq!(
        snapshot.documents[1].fetched_at_ms, 102,
        "保留首次抓到的时间"
    );
    // 上限持久计数：新修订和重启都不能放大研究次数。
    assert!(
        admin
            .begin(topic("goal-a", 9), &policy(), "v", 300)
            .unwrap()
            .is_none()
    );
    kernel.stop_all().await.unwrap();
    let (kernel, admin) = open(store.clone()).await;
    assert!(
        admin
            .begin(topic("goal-a", 10), &policy(), "v", 300)
            .unwrap()
            .is_none()
    );
    assert!(
        admin
            .begin(topic("goal-b", 1), &policy(), "v", 300)
            .unwrap()
            .is_some()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unreachable_sources_fail_without_model_requests_and_bad_outputs_save_no_knowledge() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let fetcher = FakeFetcher::new(None);
    fetcher.serve(SEED, Err(FetchFailure::HttpStatus(503)));
    let selector = FakeSelector::new(None, vec![0]);
    let extractor = FakeExtractor::new(None, |_| Ok(output_json()));
    let research = researcher(&admin, fetcher.clone(), selector.clone(), extractor.clone());
    let run = admin
        .begin(topic("goal-a", 1), &policy(), "v", 100)
        .unwrap()
        .unwrap();
    let outcome = research.research(&run, || 110).await.unwrap();
    assert!(matches!(
        outcome,
        ResearchOutcome::Failed(ResearchFailure::Fetch)
    ));
    let failed = admin.finish(&run.id, 111, outcome).unwrap();
    assert_eq!(failed.status, RunStatus::Failed(ResearchFailure::Fetch));
    assert_eq!(
        failed.discovery[0].outcome,
        FetchOutcome::Failed {
            failure: FetchFailure::HttpStatus(503)
        }
    );
    assert!(selector.requests.lock().unwrap().is_empty());
    assert!(extractor.requests.lock().unwrap().is_empty());

    // 选择器失败：记录原因，不抓取任何候选页面。
    fetcher.serve(SEED, Ok(index_page()));
    *selector.reply.lock().unwrap() =
        Box::new(|_| Err(KnowledgeError::Research(ResearchFailure::Provider)));
    let run = admin
        .begin(topic("goal-a", 2), &policy(), "v", 200)
        .unwrap()
        .unwrap();
    let outcome = research.research(&run, || 210).await.unwrap();
    assert!(matches!(
        outcome,
        ResearchOutcome::Failed(ResearchFailure::Provider)
    ));
    admin.finish(&run.id, 211, outcome).unwrap();
    assert_eq!(fetcher.urls(), [SEED, SEED]);

    // 引用不在正文中：宿主拒绝写入，研究按无效输出结束，不保存知识。
    let run = admin
        .begin(topic("goal-c", 1), &policy(), "v", 300)
        .unwrap()
        .unwrap();
    advance_to_extracting(&admin, &run);
    let forged = ExtractionOutput {
        claims: vec![claim("Each mod needs a mod.json manifest file.", None)],
        hypotheses: vec![],
    };
    assert_eq!(
        admin
            .finish(&run.id, 301, ResearchOutcome::Completed(forged))
            .err(),
        Some(KnowledgeError::InvalidInput)
    );
    // 未到提炼阶段不能带着结论完成。
    let early = admin
        .begin(topic("goal-d", 1), &policy(), "v", 400)
        .unwrap();
    assert!(early.is_none(), "goal-c 仍在进行");
    admin
        .finish(
            &run.id,
            302,
            ResearchOutcome::Failed(ResearchFailure::InvalidOutput),
        )
        .unwrap();
    let early = admin
        .begin(topic("goal-d", 1), &policy(), "v", 400)
        .unwrap()
        .unwrap();
    assert_eq!(
        admin
            .finish(&early.id, 401, ResearchOutcome::Completed(output()))
            .err(),
        Some(KnowledgeError::InvalidInput)
    );
    assert!(admin.snapshot().unwrap().entries.is_empty());
    // 结局只写一次。
    assert_eq!(
        admin
            .finish(
                &run.id,
                303,
                ResearchOutcome::Failed(ResearchFailure::Cancelled)
            )
            .err(),
        Some(KnowledgeError::Conflict)
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn candidates_and_fetches_must_come_from_the_saved_stage() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let run = admin
        .begin(topic("goal-a", 1), &policy(), "v", 100)
        .unwrap()
        .unwrap();
    let writes = store.writes();
    let invented = vec![LinkCandidate {
        url: "https://docs.example/wiki/modding/secret.html".into(),
        text: "Secret".into(),
    }];
    for (attempts, candidates) in [
        // 候选不在入口页面的链接中。
        (vec![attempt(SEED, 100, Ok(index_page()))], invented),
        // 抓取记录与入口页面不对应。
        (vec![attempt(START, 100, Ok(index_page()))], candidates()),
        // 抓取时间早于准入。
        (vec![attempt(SEED, 99, Ok(index_page()))], candidates()),
        // 页面最终 URL 不在允许范围内。
        (
            vec![attempt(
                SEED,
                100,
                Ok(page("https://evil.example/", "x", &[])),
            )],
            vec![],
        ),
    ] {
        assert_eq!(
            admin
                .advance(
                    &run.id,
                    ResearchProgress::Discovered {
                        attempts,
                        candidates
                    }
                )
                .err(),
            Some(KnowledgeError::InvalidInput)
        );
    }
    assert_eq!(
        admin
            .advance(&run.id, ResearchProgress::Selected { indices: vec![0] })
            .err(),
        Some(KnowledgeError::Conflict)
    );
    assert_eq!(store.writes(), writes, "拒绝的推进零写入");
    admin
        .advance(
            &run.id,
            ResearchProgress::Discovered {
                attempts: vec![attempt(SEED, 100, Ok(index_page()))],
                candidates: candidates(),
            },
        )
        .unwrap();
    for indices in [vec![2], vec![0, 0], vec![0, 1, 0, 1]] {
        assert_eq!(
            admin
                .advance(&run.id, ResearchProgress::Selected { indices })
                .err(),
            Some(KnowledgeError::InvalidInput)
        );
    }
    admin
        .advance(&run.id, ResearchProgress::Selected { indices: vec![1] })
        .unwrap();
    assert_eq!(
        admin
            .advance(
                &run.id,
                ResearchProgress::Fetched {
                    attempts: vec![attempt(START, 101, Ok(page(START, START_TEXT, &[])))],
                },
            )
            .err(),
        Some(KnowledgeError::InvalidInput),
        "只能抓取已选候选"
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn running_research_is_interrupted_after_restart_and_never_replayed() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    let run = admin
        .begin(topic("goal-a", 1), &policy(), "v", 100)
        .unwrap()
        .unwrap();
    advance_to_extracting(&admin, &run);
    // 进程在提炼请求途中退出：不调用 stop，直接以新内核打开同一存储。
    drop(kernel);
    let (kernel, admin) = open(store.clone()).await;
    let snapshot = admin.snapshot().unwrap();
    assert_eq!(snapshot.runs[0].status, RunStatus::Interrupted);
    assert_eq!(snapshot.runs[0].stage, ResearchStage::Extracting);
    assert_eq!(snapshot.runs[0].finished_at_ms, None, "不伪造完成时间");
    assert!(snapshot.entries.is_empty());
    assert_eq!(store.stored()["runs"][0]["status"], "Interrupted");
    assert!(
        admin
            .begin(topic("goal-a", 1), &policy(), "v", 200)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        admin
            .finish(&run.id, 201, ResearchOutcome::Completed(output()))
            .err(),
        Some(KnowledgeError::Conflict)
    );
    // 中断的研究占用次数但不阻塞其他研究。
    assert!(
        admin
            .begin(topic("goal-a", 2), &policy(), "v", 200)
            .unwrap()
            .is_some()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn unconfirmed_commits_close_the_handle_and_corrupt_state_is_refused_without_clearing() {
    let directory = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(directory.path());
    let (kernel, admin) = open(store.clone()).await;
    store.fail_before(true);
    assert_eq!(
        admin.begin(topic("goal-a", 1), &policy(), "v", 100).err(),
        Some(KnowledgeError::Storage)
    );
    store.fail_before(false);
    assert_eq!(admin.snapshot().err(), Some(KnowledgeError::Unavailable));
    kernel.stop_all().await.unwrap();

    let (kernel, admin) = open(store.clone()).await;
    assert!(
        admin.snapshot().unwrap().runs.is_empty(),
        "未提交的准入不存在"
    );
    let run = admin
        .begin(topic("goal-a", 1), &policy(), "v", 100)
        .unwrap()
        .unwrap();
    advance_to_extracting(&admin, &run);
    store.fail_after(true);
    assert_eq!(
        admin
            .finish(&run.id, 110, ResearchOutcome::Completed(output()))
            .err(),
        Some(KnowledgeError::Storage)
    );
    store.fail_after(false);
    assert_eq!(admin.snapshot().err(), Some(KnowledgeError::Unavailable));
    kernel.stop_all().await.unwrap();
    // 提交已经发生：重新打开后读到实际结果。
    let (kernel, admin) = open(store.clone()).await;
    assert_eq!(admin.snapshot().unwrap().entries.len(), 2);
    kernel.stop_all().await.unwrap();

    let original = store.raw().unwrap();
    let mut tampered: Value = serde_json::from_slice(&original).unwrap();
    tampered["entries"][0]["source"]["quote"] = json!("Each mod needs a mod.json manifest.");
    let cases = [
        serde_json::to_vec(&tampered).unwrap(),
        {
            let mut forged = serde_json::from_slice::<Value>(&original).unwrap();
            forged["documents"][1]["text"] = json!("Rewritten page text without the quote.");
            serde_json::to_vec(&forged).unwrap()
        },
        {
            let mut orphan = serde_json::from_slice::<Value>(&original).unwrap();
            orphan["runs"] = json!([]);
            serde_json::to_vec(&orphan).unwrap()
        },
        br#"{"format_version":1,"format_version":1,"runs":[],"documents":[],"entries":[]}"#
            .to_vec(),
        br#"{"format_version":2,"runs":[],"documents":[],"entries":[]}"#.to_vec(),
    ];
    for (index, bytes) in cases.into_iter().enumerate() {
        store.replace(bytes.clone());
        let kernel = eve_kernel::Kernel::with_services(eve_kernel::KernelServices {
            state: store.clone(),
            ..eve_kernel::KernelServices::default()
        });
        kernel
            .register(Box::new(
                eve_knowledge_plugin::KnowledgePlugin::new().unwrap(),
            ))
            .unwrap();
        let error = kernel.start_all().await.unwrap_err().to_string();
        let expected = if index == 4 {
            "不支持该领域知识状态版本"
        } else {
            "领域知识状态损坏；未清空"
        };
        assert!(error.contains(expected), "{index}: {error}");
        assert!(!error.contains("private-storage-detail"));
        assert_eq!(store.raw().unwrap(), bytes, "损坏状态保留原字节");
    }
}
