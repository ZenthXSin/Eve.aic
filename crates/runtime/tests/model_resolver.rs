#[path = "support/session.rs"]
mod fixture;
use eve_llm_api::*;
use eve_plugin_api::ServiceId;
use eve_runtime::{
    ContextBinding, LlmHost, LlmHostConfig, SessionBinding, SessionLlmHost, SessionRunError,
};
use fixture::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Resolver {
    provider: Arc<Provider>,
    calls: AtomicUsize,
    mode: AtomicUsize,
}
impl LlmModelResolver for Resolver {
    fn resolve(&self) -> Result<ModelSelection, LlmError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.mode.load(Ordering::SeqCst) {
            0 => Err(LlmError::Configuration("无效模型选择".into())),
            1 => panic!("测试解析异常"),
            mode => Ok(ModelSelection {
                provider: self.provider.clone(),
                provider_timeout: match mode {
                    2 => Duration::ZERO,
                    3 => Duration::from_millis(20),
                    _ => Duration::from_secs(1),
                },
            }),
        }
    }
}
fn host(rig: &Rig, resolver: Arc<Resolver>) -> LlmHost {
    LlmHost::new(
        Provider::new(vec![]),
        rig.registry.clone(),
        rig.kernel.clone(),
        rig.permissions.clone(),
        ContextBinding {
            service_id: ServiceId::new(CONTEXT).unwrap(),
            expected_owner: id(OWNER),
        },
        vec![ToolBinding {
            name: "receipt".into(),
            service_id: ServiceId::new(TOOL).unwrap(),
            expected_owner: id(OWNER),
        }],
        LlmHostConfig::default(),
    )
    .unwrap()
    .with_model_resolver(resolver)
}

#[tokio::test]
async fn selection_is_resolved_once_before_pending_and_timeout_is_used() {
    let provider = Provider::new(vec![
        Step::new(calls()),
        Step::new(final_response("已完成")),
        Step::blocked(Arc::new(tokio::sync::Notify::new())),
        Step::new(final_response("普通宿主完成")),
    ]);
    let store = Arc::new(FaultStore::default());
    let rig = Rig::new(
        Provider::new(vec![]),
        store.clone(),
        LlmHostConfig::default(),
    )
    .await;
    let resolver = Arc::new(Resolver {
        provider: provider.clone(),
        calls: AtomicUsize::new(0),
        mode: AtomicUsize::new(0),
    });
    let session = SessionLlmHost::new(host(&rig, resolver.clone()), SessionBinding::builtin());
    let before = store.bytes();
    for mode in 0..3 {
        resolver.mode.store(mode, Ordering::SeqCst);
        assert!(matches!(
            session.run_turn(input("dynamic", "拒绝")).await,
            Err(SessionRunError::NotStarted(_))
        ));
        assert_eq!(store.bytes(), before);
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(rig.starts.load(Ordering::SeqCst), 0);
    }
    resolver.mode.store(4, Ordering::SeqCst);
    let completed = session
        .run_turn(input("dynamic", "工具请求"))
        .await
        .unwrap();
    assert_eq!(completed.output.text, "已完成");
    assert_eq!(completed.output.diagnostics.provider_requests, 2);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 4);
    resolver.mode.store(3, Ordering::SeqCst);
    assert!(
        matches!(session.run_turn(input("dynamic", "限时请求")).await,
        Err(SessionRunError::Turn(failure)) if failure.error == LlmError::ProviderTimeout)
    );
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 5);
    resolver.mode.store(4, Ordering::SeqCst);
    let output = host(&rig, resolver.clone())
        .run_turn(TurnInput {
            text: "普通请求".into(),
        })
        .await
        .unwrap();
    assert_eq!(output.text, "普通宿主完成");
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 6);
    assert_eq!(provider.requests.lock().unwrap().len(), 4);
    rig.stop().await;
}
