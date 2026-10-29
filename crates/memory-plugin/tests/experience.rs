// 本验收只复用文件故障注入器；同一测试夹具里的Session助手供其他验收使用。
#[allow(dead_code)]
mod support;
use eve_kernel::{Kernel, KernelServices};
use eve_memory_api::*;
use eve_memory_plugin::{EXPERIENCE_STATE_KEY, ExperienceMemoryController, ExperienceMemoryPlugin};
use eve_plugin_api::{PluginId, StateStore};

fn context(session: &str, user: &str) -> ExperienceReadContext {
    ExperienceReadContext {
        scope: MemoryScope {
            channel: "qq".into(),
            session_id: session.into(),
            user_id: user.into(),
        },
        principal: Some(user.into()),
    }
}
fn local(
    c: &ExperienceReadContext,
    operation: &str,
    text: &str,
    revision: u64,
    status: ExperienceStatus,
) -> ExperienceVersion {
    ExperienceVersion {
        revision,
        operation_id: operation.into(),
        at_ms: revision,
        expires_at_ms: None,
        text: text.into(),
        source: ExperienceSource::ScopedEvidence {
            scope: c.scope.clone(),
            reference: format!("receipt-{operation}"),
            revision,
        },
        status,
    }
}
fn public(
    operation: &str,
    text: &str,
    revision: u64,
    status: ExperienceStatus,
) -> ExperienceVersion {
    ExperienceVersion {
        revision,
        operation_id: operation.into(),
        at_ms: revision,
        expires_at_ms: None,
        text: text.into(),
        source: ExperienceSource::PublicArtifact {
            reference: format!("public-{operation}"),
            revision,
        },
        status,
    }
}
async fn open(services: KernelServices) -> (Kernel, ExperienceMemoryController) {
    let kernel = Kernel::with_services(services);
    let plugin = ExperienceMemoryPlugin::new().unwrap();
    let controller = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, controller)
}
#[tokio::test]
async fn public_experience_reuses_across_windows_but_private_quotes_never_promote() {
    let (kernel, c) = open(KernelServices::default()).await;
    let private = context("private:a", "a");
    let group = context("group:g", "a");
    let other = context("private:b", "b");
    c.save_local(
        &private,
        0,
        "private",
        local(
            &private,
            "private-add",
            "Rust 私聊原话 secret",
            1,
            ExperienceStatus::Unverified,
        ),
    )
    .unwrap();
    c.save_local(
        &group,
        1,
        "group",
        local(
            &group,
            "group-add",
            "Rust 群聊原话 secret",
            1,
            ExperienceStatus::Unverified,
        ),
    )
    .unwrap();
    c.publish_public(
        2,
        "public",
        public(
            "public-add",
            "Rust 实践先运行 cargo check",
            1,
            ExperienceStatus::Verified,
        ),
    )
    .unwrap();
    assert_eq!(c.recall(&private, "Rust", 8).unwrap().len(), 2);
    let group_hits = c.recall(&group, "Rust", 8).unwrap();
    assert_eq!(group_hits.len(), 2);
    assert!(group_hits.iter().all(|h| h.id != "private"));
    let other_hits = c.recall(&other, "Rust", 8).unwrap();
    assert_eq!(other_hits.len(), 1);
    assert_eq!(other_hits[0].id, "public");
    assert!(c.recall(&other, "trust", 3).unwrap().is_empty());
    assert!(
        c.publish_public(
            3,
            "leak",
            local(
                &private,
                "bad-promotion",
                "Rust 私聊secret",
                1,
                ExperienceStatus::Verified
            )
        )
        .is_err()
    );
    assert!(
        c.revise(
            &other,
            3,
            "private",
            local(
                &other,
                "bad-edit",
                "Rust stolen",
                2,
                ExperienceStatus::Unverified
            )
        )
        .is_err()
    );
    assert_eq!(c.snapshot().unwrap().revision, 3);
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn personal_requires_host_identity_and_never_uses_display_name() {
    let (kernel, c) = open(KernelServices::default()).await;
    let private = context("private:a", "authenticated-a");
    c.save_personal(
        &private,
        0,
        "personal",
        local(
            &private,
            "personal-add",
            "Rust 示例偏好",
            1,
            ExperienceStatus::Unverified,
        ),
    )
    .unwrap();
    assert_eq!(
        c.recall(&context("other-window", "authenticated-a"), "Rust", 3)
            .unwrap()
            .len(),
        1
    );
    assert!(
        c.recall(&context("other-window", "display-name-a"), "Rust", 3)
            .unwrap()
            .is_empty()
    );
    let mut no_identity = private.clone();
    no_identity.principal = None;
    assert!(c.recall(&no_identity, "Rust", 3).unwrap().is_empty());
    assert!(
        c.save_personal(
            &no_identity,
            1,
            "no-id",
            local(&private, "no-id", "Rust", 1, ExperienceStatus::Unverified)
        )
        .is_err()
    );
    kernel.stop_all().await.unwrap();
}
#[tokio::test]
async fn correction_invalidates_verification_and_revocation_retains_audit_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = support::RecordingStore::open(dir.path());
    let (kernel, c) = open(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    })
    .await;
    c.publish_public(
        0,
        "general",
        public("add", "Rust old method", 1, ExperienceStatus::Verified),
    )
    .unwrap();
    // 已验证状态不能转移给修改后的正文。
    assert!(
        c.revise_public(
            1,
            "general",
            public(
                "bad-correct",
                "Rust new method",
                2,
                ExperienceStatus::Verified
            )
        )
        .is_err()
    );
    c.revise_public(
        1,
        "general",
        public(
            "correct",
            "Rust new method",
            2,
            ExperienceStatus::Unverified,
        ),
    )
    .unwrap();
    assert!(
        c.recall(&context("new", "new"), "old", 3)
            .unwrap()
            .is_empty()
    );
    assert!(
        c.recall(&context("new", "new"), "new", 3).unwrap()[0]
            .version
            .status
            == ExperienceStatus::Unverified
    );
    c.revise_public(
        2,
        "general",
        public("verify", "Rust new method", 3, ExperienceStatus::Verified),
    )
    .unwrap();
    c.revise_public(
        3,
        "general",
        public("revoke", "Rust new method", 4, ExperienceStatus::Revoked),
    )
    .unwrap();
    assert!(
        c.recall(&context("new", "new"), "Rust", 3)
            .unwrap()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
    assert!(c.snapshot().is_err());
    let (reopened, recovered) = open(KernelServices {
        state: store,
        ..KernelServices::default()
    })
    .await;
    let snapshot = recovered.snapshot().unwrap();
    assert_eq!(snapshot.records[0].history.len(), 4);
    assert!(
        recovered
            .recall(&context("new", "new"), "Rust", 3)
            .unwrap()
            .is_empty()
    );
    assert!(
        recovered
            .revise_public(
                4,
                "general",
                public("restore", "Rust new method", 5, ExperienceStatus::Verified)
            )
            .is_err()
    );
    reopened.stop_all().await.unwrap();
}
#[tokio::test]
async fn storage_confirmation_loss_closes_service_and_recovery_keeps_committed_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = support::RecordingStore::open(dir.path());
    let (kernel, c) = open(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    })
    .await;
    store.fail_after_writes(true);
    assert!(
        c.publish_public(
            0,
            "general",
            public("add", "Rust cargo check", 1, ExperienceStatus::Verified)
        )
        .is_err()
    );
    assert!(c.snapshot().is_err());
    kernel.stop_all().await.unwrap();
    store.fail_after_writes(false);
    let (reopened, recovered) = open(KernelServices {
        state: store,
        ..KernelServices::default()
    })
    .await;
    assert_eq!(
        recovered
            .recall(&context("new", "new"), "Rust", 3)
            .unwrap()
            .len(),
        1
    );
    reopened.stop_all().await.unwrap();
}
#[tokio::test]
async fn corrupt_or_unknown_version_state_is_preserved_and_refused() {
    for bytes in [
        br#"{"format_version":2,"state":{"revision":0,"records":[]}}"#.to_vec(),
        br#"{"format_version":1,"state":{"revision":1,"records":[]}}"#.to_vec(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = support::RecordingStore::open(dir.path());
        let id = PluginId::new(EXPERIENCE_MEMORY_PLUGIN_ID).unwrap();
        store
            .set(&id, EXPERIENCE_STATE_KEY.into(), bytes.clone())
            .unwrap();
        let kernel = Kernel::with_services(KernelServices {
            state: store.clone(),
            ..KernelServices::default()
        });
        kernel
            .register(Box::new(ExperienceMemoryPlugin::new().unwrap()))
            .unwrap();
        assert!(kernel.start_all().await.is_err());
        assert_eq!(store.get(&id, EXPERIENCE_STATE_KEY).unwrap(), Some(bytes));
        kernel.stop_all().await.unwrap();
    }
}
#[tokio::test]
async fn chinese_queries_archive_and_capacity_have_explicit_semantics() {
    let (kernel, c) = open(KernelServices::default()).await;
    let ctx = context("private", "a");
    for i in 0..MAX_EXPERIENCES {
        c.save_local(
            &ctx,
            i as u64,
            &format!("id-{i}"),
            local(
                &ctx,
                &format!("op-{i}"),
                "先检查编译错误再运行",
                1,
                ExperienceStatus::Unverified,
            ),
        )
        .unwrap();
    }
    assert!(!c.recall(&ctx, "如何修复编译错误", 3).unwrap().is_empty());
    assert!(
        c.save_local(
            &ctx,
            MAX_EXPERIENCES as u64,
            "overflow",
            local(&ctx, "overflow", "编译", 1, ExperienceStatus::Unverified)
        )
        .is_err()
    );
    c.revise(
        &ctx,
        MAX_EXPERIENCES as u64,
        "id-0",
        local(
            &ctx,
            "archive",
            "先检查编译错误再运行",
            2,
            ExperienceStatus::Archived,
        ),
    )
    .unwrap();
    assert!(
        c.recall(&ctx, "编译错误", 8)
            .unwrap()
            .iter()
            .all(|h| h.id != "id-0")
    );
    assert_eq!(c.snapshot().unwrap().records.len(), MAX_EXPERIENCES);
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn full_version_capacity_reserves_revocation_and_replay_precedes_stale_cas() {
    let (kernel, c) = open(KernelServices::default()).await;
    let original = public(
        "first",
        "Rust reusable method",
        1,
        ExperienceStatus::Unverified,
    );
    c.publish_public(0, "general", original.clone()).unwrap();
    // 相同操作重放成功；不能借重放修改正文或向其他记录复制相同操作ID。
    c.publish_public(0, "general", original.clone()).unwrap();
    assert_eq!(c.snapshot().unwrap().revision, 1);
    let mut changed = original.clone();
    changed.text = "Rust silently changed".into();
    assert!(c.publish_public(1, "general", changed).is_err());
    assert!(c.publish_public(1, "different", original).is_err());
    for revision in 2..MAX_EXPERIENCE_VERSIONS {
        c.revise_public(
            (revision - 1) as u64,
            "general",
            public(
                &format!("op-{revision}"),
                "Rust reusable method",
                revision as u64,
                ExperienceStatus::Unverified,
            ),
        )
        .unwrap();
    }
    let full = (MAX_EXPERIENCE_VERSIONS - 1) as u64;
    assert!(
        c.revise_public(
            full,
            "general",
            public(
                "overflow",
                "Rust reusable method",
                full + 1,
                ExperienceStatus::Unverified
            )
        )
        .is_err()
    );
    c.revise_public(
        full,
        "general",
        public(
            "reserved-revoke",
            "Rust reusable method",
            full + 1,
            ExperienceStatus::Revoked,
        ),
    )
    .unwrap();
    assert_eq!(
        c.snapshot().unwrap().records[0].history.len(),
        MAX_EXPERIENCE_VERSIONS
    );
    assert!(
        c.recall(&context("new", "new"), "Rust", 3)
            .unwrap()
            .is_empty()
    );
    kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn expiration_filters_recall_and_hits_without_deleting_original_versions() {
    let store = std::sync::Arc::new(eve_kernel::backends::MemoryStateStore::default());
    let (kernel, c) = open(KernelServices {
        state: store.clone(),
        ..KernelServices::default()
    })
    .await;
    let access = context("window-a", "principal-a");
    let mut version = local(
        &access,
        "expiring",
        "atlas evidence",
        1,
        ExperienceStatus::Unverified,
    );
    version.expires_at_ms = Some(2);
    assert!(!version.is_expired_at(1));
    assert!(version.is_expired_at(2));
    c.save_personal(&access, 0, "expiring", version.clone())
        .unwrap();
    assert!(c.recall(&access, "atlas", 3).unwrap().is_empty());
    let forged = ExperienceHit {
        id: "expiring".into(),
        visibility: ExperienceVisibility::Principal {
            principal: "principal-a".into(),
        },
        version,
        score: 1,
    };
    assert!(forged.validate_for(&access).is_err());
    let before = c.snapshot().unwrap();
    kernel.stop_all().await.unwrap();
    let (kernel, c) = open(KernelServices {
        state: store,
        ..KernelServices::default()
    })
    .await;
    assert!(c.recall(&access, "atlas", 3).unwrap().is_empty());
    assert!(c.snapshot().unwrap() == before);
    // 不带新字段的旧数据继续解码为无期限，不能被自动清空。
    let mut raw = serde_json::to_value(&before.records[0].history[0]).unwrap();
    raw.as_object_mut().unwrap().remove("expires_at_ms");
    assert!(
        serde_json::from_value::<ExperienceVersion>(raw)
            .unwrap()
            .expires_at_ms
            .is_none()
    );
    kernel.stop_all().await.unwrap();
}
