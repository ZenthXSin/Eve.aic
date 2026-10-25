use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_llm_api::ContextScope;
use eve_plugin_api::*;
use eve_training_api::*;
use eve_training_plugin::TrainingPlugin;
use eve_training_sqlite::SqliteExpressions;
use std::{fs, sync::Arc};

fn scope(user: &str) -> ContextScope {
    ContextScope {
        session_id: "group".into(),
        user_id: user.into(),
    }
}
fn row(index: usize, user: &str) -> ExpressionRecord {
    ExpressionRecord {
        scope: scope(user),
        id: format!("{index:064x}"),
        body: "a".repeat(64),
        chars: index % 512 + 1,
        paragraphs: 1,
        question: index % 2 == 0,
        formal: false,
        casual: true,
        active: true,
    }
}
#[test]
fn large_import_and_scoped_statistics_survive_reset_restart_and_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("expressions.sqlite3");
    let legacy: Vec<_> = (0..2709).map(|n| row(n, "one")).collect();
    let db = SqliteExpressions::open(&path).unwrap();
    db.import_legacy(&legacy).unwrap();
    for n in 2709..5000 {
        assert!(db.insert(&row(n, "one")).unwrap());
    }
    let one = db.snapshot(Some(&scope("one"))).unwrap().unwrap();
    assert_eq!(one.samples, 5000);
    assert!(!db.insert(&row(4999, "one")).unwrap());
    let mut conflict = row(4999, "one");
    conflict.body = "b".repeat(64);
    assert!(db.insert(&conflict).is_err());
    assert_eq!(
        db.get(&scope("one"), &conflict.id).unwrap().unwrap().body,
        "a".repeat(64)
    );
    db.insert(&row(0, "two")).unwrap();
    assert_eq!(db.snapshot(Some(&scope("one"))).unwrap().unwrap(), one);
    db.reset(&scope("one")).unwrap();
    assert!(db.snapshot(Some(&scope("one"))).unwrap().is_none());
    assert_eq!(db.snapshot(None).unwrap().unwrap().samples, 1);
    drop(db);
    let restored = SqliteExpressions::open(&path).unwrap();
    restored.import_legacy(&legacy).unwrap();
    // Even a changed legacy digest must not reactivate reset rows.
    restored.import_legacy(&legacy[..2708]).unwrap();
    assert!(
        !restored
            .get(&scope("one"), &row(0, "one").id)
            .unwrap()
            .unwrap()
            .active
    );
    assert!(restored.snapshot(Some(&scope("one"))).unwrap().is_none());
    assert_eq!(
        restored
            .snapshot(Some(&scope("two")))
            .unwrap()
            .unwrap()
            .samples,
        1
    );
}
#[test]
fn aggregation_matches_exact_quantiles_and_percentages() {
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteExpressions::open(&dir.path().join("db")).unwrap();
    for (n, chars) in [10, 60, 61, 300].into_iter().enumerate() {
        let mut value = row(n, "one");
        value.chars = chars;
        value.paragraphs = if n == 3 { 2 } else { 1 };
        value.formal = n == 0;
        db.insert(&value).unwrap();
    }
    let stats = db.snapshot(None).unwrap().unwrap();
    assert_eq!(
        (stats.samples, stats.median_chars, stats.p75_chars),
        (4, 60, 61)
    );
    assert_eq!(
        (
            stats.short_percent,
            stats.single_paragraph_percent,
            stats.question_percent,
            stats.formal_percent,
            stats.casual_percent
        ),
        (50, 75, 50, 25, 100)
    );
}
#[test]
fn missing_mismatched_or_corrupt_files_are_not_recreated_or_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite3");
    drop(SqliteExpressions::open(&path).unwrap());
    let bytes = fs::read(&path).unwrap();
    let marker = path.with_extension("required");
    let identity = fs::read(&marker).unwrap();
    fs::remove_file(&path).unwrap();
    assert!(SqliteExpressions::open(&path).is_err());
    assert!(!path.exists());
    fs::write(&path, &bytes).unwrap();
    fs::write(&marker, b"wrong").unwrap();
    assert!(SqliteExpressions::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    fs::write(&marker, identity).unwrap();
    fs::write(&path, b"corrupt preserved bytes").unwrap();
    assert!(SqliteExpressions::open(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"corrupt preserved bytes");
}
fn service(registry: &Arc<dyn ServiceRegistry>) -> Arc<dyn TrainingService> {
    registry
        .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
        .unwrap()
        .unwrap()
        .value
        .downcast::<TrainingServiceHandle>()
        .unwrap()
        .0
        .clone()
}
#[tokio::test]
async fn full_legacy_json_is_preserved_while_new_samples_keep_learning() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(MemoryStateStore::default());
    let id = PluginId::new(TRAINING_PLUGIN_ID).unwrap();
    let mut legacy: Vec<_> = (0..2709).map(|n| row(n, "one")).collect();
    // Long valid scope IDs reproduce the production 1 MiB byte limit.
    for r in &mut legacy {
        r.scope.session_id = "g".repeat(105);
    }
    let bytes = serde_json::to_vec(&serde_json::json!({"version":1,"rows":legacy})).unwrap();
    assert!(bytes.len() > 1_000_000 && bytes.len() < 1_048_576);
    state
        .set(&id, "expression.v1".into(), bytes.clone())
        .unwrap();
    let backends = KernelServices {
        state: state.clone(),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    kernel
        .register(Box::new(
            TrainingPlugin::new(true)
                .unwrap()
                .with_phrase_learning()
                .with_expression_store(Arc::new(
                    SqliteExpressions::open(&dir.path().join("db")).unwrap(),
                )),
        ))
        .unwrap();
    kernel.start(&id).await.unwrap();
    let training = service(&registry);
    for n in 0..PHRASE_BATCH {
        training
            .observe_user_message(
                &scope("new-user"),
                &format!("new-{n}"),
                &format!("哈哈，这游戏真好玩{n}"),
            )
            .unwrap();
    }
    assert_eq!(
        training
            .expression_snapshot(&scope("new-user"))
            .unwrap()
            .unwrap()
            .samples,
        PHRASE_BATCH
    );
    let batch = training.take_phrase_batch(1).unwrap().unwrap();
    assert_eq!(batch.scope, scope("new-user"));
    training
        .observe_user_message(&scope("new-user"), "new-0", "哈哈，这游戏真好玩0")
        .unwrap();
    assert!(training.take_phrase_batch(2).unwrap().is_none());
    assert_eq!(state.get(&id, "expression.v1").unwrap().unwrap(), bytes);
    training.reset_expression(&scope("new-user")).unwrap();
    kernel.stop(&id).await.unwrap();
    kernel.start(&id).await.unwrap();
    assert!(
        service(&registry)
            .expression_snapshot(&scope("new-user"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        service(&registry)
            .expression_baseline()
            .unwrap()
            .unwrap()
            .samples,
        2709
    );
    kernel.stop_all().await.unwrap();
}
