use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_people_api::*;
use eve_people_plugin::*;
use eve_plugin_api::*;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

fn id(n: usize) -> String {
    format!("qq:{n:064x}")
}
fn scope(scene: usize, user: usize) -> PersonScope {
    PersonScope {
        conversation: id(scene),
        participant: id(user),
    }
}
fn name(value: &str, source: NameSource) -> Option<PersonName> {
    Some(PersonName {
        value: value.into(),
        source,
    })
}
fn plugin_id() -> PluginId {
    PluginId::new(PEOPLE_PLUGIN_ID).unwrap()
}
async fn start(state: Arc<dyn StateStore>) -> (Kernel, PeopleController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = PeoplePlugin::new().unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&plugin_id()).await.unwrap();
    (kernel, controller)
}
#[tokio::test]
async fn scope_names_corrections_precedence_and_restart_keep_identity() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, people) = start(state.clone()).await;
    people
        .observe(scope(1, 11), name("小明", NameSource::Platform))
        .unwrap();
    people
        .observe(scope(1, 12), name("小明", NameSource::Declared))
        .unwrap();
    people
        .observe(scope(2, 13), name("别群私有名字", NameSource::Declared))
        .unwrap();
    people
        .observe(scope(3, 14), name("私聊称呼", NameSource::Declared))
        .unwrap();
    let view = people.view(&id(11)).unwrap().unwrap();
    assert_eq!(view.others.len(), 1);
    assert_eq!(view.others[0].scope.participant, id(12));
    people
        .observe(scope(1, 11), name("阿白", NameSource::Declared))
        .unwrap();
    people
        .observe(scope(1, 11), name("平台后来改名", NameSource::Platform))
        .unwrap();
    let current = people.view(&id(11)).unwrap().unwrap().current;
    assert_eq!(current.name.unwrap().value, "阿白");
    assert_eq!(current.revision, 2);
    let before = state.get(&plugin_id(), "people.v1").unwrap().unwrap();
    kernel.stop(&plugin_id()).await.unwrap();
    assert!(people.view(&id(11)).is_err());
    let (_restored, restored) = start(state.clone()).await;
    assert_eq!(
        restored
            .view(&id(11))
            .unwrap()
            .unwrap()
            .current
            .name
            .unwrap()
            .value,
        "阿白"
    );
    assert_eq!(
        state.get(&plugin_id(), "people.v1").unwrap().unwrap(),
        before
    );
}
struct Faulty {
    base: MemoryStateStore,
    failure: AtomicU8,
}
impl StateStore for Faulty {
    fn get(&self, ns: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.base.get(ns, key)
    }
    fn set(&self, ns: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        let mode = self.failure.load(Ordering::SeqCst);
        if mode != 1 {
            self.base.set(ns, key, value)?;
        }
        if mode == 0 {
            Ok(())
        } else {
            Err(PluginError::State("synthetic uncertain commit".into()))
        }
    }
}
#[tokio::test]
async fn failed_writes_close_handles_and_reopen_actual_state_without_replay() {
    for mode in [1, 2] {
        let state = Arc::new(Faulty {
            base: MemoryStateStore::default(),
            failure: AtomicU8::new(0),
        });
        let (kernel, people) = start(state.clone()).await;
        people
            .observe(scope(1, 11), name("旧称呼", NameSource::Declared))
            .unwrap();
        state.failure.store(mode, Ordering::SeqCst);
        assert!(
            people
                .observe(scope(1, 11), name("新称呼", NameSource::Declared))
                .is_err()
        );
        assert!(people.view(&id(11)).is_err());
        kernel.stop(&plugin_id()).await.unwrap();
        state.failure.store(0, Ordering::SeqCst);
        let before = state.get(&plugin_id(), "people.v1").unwrap();
        let (_kernel, people) = start(state.clone()).await;
        assert_eq!(
            people
                .view(&id(11))
                .unwrap()
                .unwrap()
                .current
                .name
                .unwrap()
                .value,
            if mode == 1 { "旧称呼" } else { "新称呼" }
        );
        assert_eq!(state.get(&plugin_id(), "people.v1").unwrap(), before);
    }
}
#[tokio::test]
async fn corrupt_and_unknown_documents_are_not_replaced() {
    for data in [
        r#"{"version":4,"people":[]}"#,
        r#"{"version":3,"people":[]}"#,
        r#"{"version":1,"version":1,"people":[]}"#,
        r#"{"version":1,"people":[],"extra":true}"#,
        "not json",
    ] {
        let state = Arc::new(MemoryStateStore::default());
        state
            .set(&plugin_id(), "people.v1".into(), data.as_bytes().to_vec())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(PeoplePlugin::new().unwrap()))
            .unwrap();
        assert!(kernel.start(&plugin_id()).await.is_err());
        assert_eq!(
            state.get(&plugin_id(), "people.v1").unwrap().unwrap(),
            data.as_bytes()
        );
    }
}
#[tokio::test]
async fn capacity_keeps_existing_records_and_group_views_are_bounded() {
    let state = Arc::new(MemoryStateStore::default());
    let records: Vec<_> = (0..MAX_PEOPLE)
        .map(|n| Person {
            scope: scope(1, n + 10),
            name: name("同名", NameSource::Declared),
            revision: 1,
            impressions: vec![],
            pending: vec![],
            presence: None,
        })
        .collect();
    state
        .set(
            &plugin_id(),
            "people.v1".into(),
            serde_json::to_vec(&serde_json::json!({"version":1,"people":records})).unwrap(),
        )
        .unwrap();
    let (_kernel, people) = start(state.clone()).await;
    let before = state.get(&plugin_id(), "people.v1").unwrap();
    assert!(people.observe(scope(1, 99999), None).is_err());
    let view = people.view(&id(10)).unwrap().unwrap();
    assert_eq!(view.others.len(), MAX_VISIBLE_PEOPLE);
    assert!(view.truncated);
    assert_eq!(state.get(&plugin_id(), "people.v1").unwrap(), before);
}
async fn start_learning(state: Arc<dyn StateStore>) -> (Kernel, PeopleController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = PeoplePlugin::new().unwrap().with_impression_learning();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start(&plugin_id()).await.unwrap();
    (kernel, controller)
}
fn draft(text: &str, quote: &str, replaces: Option<&str>) -> ImpressionDraft {
    ImpressionDraft {
        text: text.into(),
        quote: quote.into(),
        replaces: replaces.map(Into::into),
    }
}
#[tokio::test]
async fn impressions_need_verbatim_quotes_stay_with_the_speaker_and_keep_format_one_until_used() {
    let state = Arc::new(MemoryStateStore::default());
    let (_kernel, people) = start_learning(state.clone()).await;
    people.observe(scope(1, 2), None).unwrap();
    people
        .observe(scope(1, 3), name("阿白", NameSource::Declared))
        .unwrap();
    let saved = |state: &MemoryStateStore| -> serde_json::Value {
        serde_json::from_slice(&state.get(&plugin_id(), "people.v1").unwrap().unwrap()).unwrap()
    };
    assert_eq!(saved(&state)["version"], 1, "没有印象时仍是格式 1");
    for text in ["我在学Rust", "我养猫", "我住北京", "我喜欢爬山"] {
        people.note(&id(2), text).unwrap();
    }
    assert_eq!(saved(&state)["version"], 2);
    let batch = people.take_impression_batch().unwrap().unwrap();
    assert!(
        people.take_impression_batch().unwrap().is_none(),
        "同一人在途时不再取批"
    );
    assert!(
        people
            .finish_impression_batch(&batch, vec![draft("是医生", "我是医生", None)], 1)
            .is_err()
    );
    assert!(
        people
            .finish_impression_batch(&batch, vec![draft("在学 Rust", "我在学Rust", None)], 1)
            .is_err(),
        "批次已结束"
    );
    for text in ["我在学Rust", "我养猫", "我住北京", "我喜欢爬山"] {
        people.note(&id(2), text).unwrap();
    }
    let batch = people.take_impression_batch().unwrap().unwrap();
    assert_eq!(
        people
            .finish_impression_batch(
                &batch,
                vec![
                    draft("在学 Rust", "我在学Rust", None),
                    draft("养猫", "我养猫", None)
                ],
                1
            )
            .unwrap(),
        2
    );
    let view = people.view(&id(3)).unwrap().unwrap();
    assert!(view.current.impressions.is_empty());
    assert!(
        view.others
            .iter()
            .all(|p| p.impressions.is_empty() && p.pending.is_empty())
    );
    let mine = people.view(&id(2)).unwrap().unwrap().current;
    assert_eq!(mine.impressions.len(), 2);
    assert!(mine.pending.is_empty());
    assert!(
        !people
            .forget_impression(&id(3), &mine.impressions[0].id)
            .unwrap()
    );
    assert!(
        people
            .forget_impression(&id(2), &mine.impressions[0].id)
            .unwrap()
    );
    drop(people);
    // 格式 1 里出现印象、编号超出计数都视为损坏，拒绝启动且不改写。
    let mut bad = saved(&state);
    bad["next"] = 1.into();
    let bytes = serde_json::to_vec(&bad).unwrap();
    let corrupt = Arc::new(MemoryStateStore::default());
    corrupt
        .set(&plugin_id(), "people.v1".into(), bytes.clone())
        .unwrap();
    let kernel = Kernel::with_services(KernelServices {
        state: corrupt.clone(),
        ..KernelServices::default()
    });
    kernel
        .register(Box::new(PeoplePlugin::new().unwrap()))
        .unwrap();
    assert!(kernel.start(&plugin_id()).await.is_err());
    assert_eq!(
        corrupt.get(&plugin_id(), "people.v1").unwrap().unwrap(),
        bytes
    );
}

#[tokio::test]
async fn visits_count_messages_keep_the_previous_visit_and_upgrade_only_when_used() {
    let state = Arc::new(MemoryStateStore::default());
    let (kernel, people) = start(state.clone()).await;
    people
        .observe(scope(1, 11), name("小明", NameSource::Platform))
        .unwrap();
    let version = |state: &MemoryStateStore| {
        let bytes = state.get(&plugin_id(), "people.v1").unwrap().unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["version"].clone()
    };
    assert_eq!(version(&state), 1, "没有来访记录时仍写格式 1");
    assert!(
        people.visit(&id(99), Scene::Private, 1).is_err(),
        "未见过的人不记"
    );
    people.visit(&id(11), Scene::Group, 1_000).unwrap();
    let first = people
        .view(&id(11))
        .unwrap()
        .unwrap()
        .current
        .presence
        .unwrap();
    assert_eq!((first.messages, first.previous_seen_ms), (1, None));
    people.visit(&id(11), Scene::Group, 5_000).unwrap();
    // 时钟倒退只计数，时间不往回走。
    people.visit(&id(11), Scene::Group, 3_000).unwrap();
    assert!(
        people.visit(&id(11), Scene::Private, 6_000).is_err(),
        "场景随身份固定"
    );
    let seen = people
        .view(&id(11))
        .unwrap()
        .unwrap()
        .current
        .presence
        .unwrap();
    assert_eq!(
        (
            seen.first_seen_ms,
            seen.last_seen_ms,
            seen.previous_seen_ms,
            seen.messages
        ),
        (1_000, 5_000, Some(5_000), 3)
    );
    assert_eq!(version(&state), 3);
    people
        .observe(scope(1, 12), name("阿白", NameSource::Declared))
        .unwrap();
    people.visit(&id(12), Scene::Group, 2_000).unwrap();
    let view = people.view(&id(11)).unwrap().unwrap();
    assert!(
        view.others.iter().all(|p| p.presence.is_none()),
        "别人的来访情况不给看"
    );
    kernel.stop(&plugin_id()).await.unwrap();
    let (_kernel, people) = start(state.clone()).await;
    assert_eq!(
        people
            .view(&id(11))
            .unwrap()
            .unwrap()
            .current
            .presence
            .unwrap()
            .messages,
        3
    );
}
#[tokio::test]
async fn invalid_visit_records_refuse_to_start_and_keep_the_bytes() {
    for presence in [
        serde_json::json!({"scene":"private","first_seen_ms":5,"last_seen_ms":1,"messages":1}),
        serde_json::json!({"scene":"private","first_seen_ms":1,"last_seen_ms":2,"messages":2}),
        serde_json::json!({"scene":"private","first_seen_ms":1,"last_seen_ms":2,"previous_seen_ms":1,"messages":1}),
    ] {
        let state = Arc::new(MemoryStateStore::default());
        let bytes = serde_json::to_vec(&serde_json::json!({"version":3,"people":[{
            "scope":{"conversation":id(1),"participant":id(11)},"name":null,"revision":1,"presence":presence}]}))
        .unwrap();
        state
            .set(&plugin_id(), "people.v1".into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: state.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(PeoplePlugin::new().unwrap()))
            .unwrap();
        assert!(kernel.start(&plugin_id()).await.is_err());
        assert_eq!(
            state.get(&plugin_id(), "people.v1").unwrap().unwrap(),
            bytes
        );
    }
    // 格式 2 不能带来访记录。
    let state = Arc::new(MemoryStateStore::default());
    state
        .set(
            &plugin_id(),
            "people.v1".into(),
            serde_json::to_vec(&serde_json::json!({"version":2,"people":[{
        "scope":{"conversation":id(1),"participant":id(11)},"name":null,"revision":1,
        "presence":{"scene":"private","first_seen_ms":1,"last_seen_ms":1,"messages":1}}],"next":0}))
            .unwrap(),
        )
        .unwrap();
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    kernel
        .register(Box::new(PeoplePlugin::new().unwrap()))
        .unwrap();
    assert!(kernel.start(&plugin_id()).await.is_err());
}
