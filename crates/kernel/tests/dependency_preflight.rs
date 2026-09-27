use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginDependency, PluginError, PluginFuture, PluginId,
    PluginManifest, PluginState, StateStore, cleanup,
};
use std::sync::{Arc, Mutex};

type Trace = Arc<Mutex<Vec<String>>>;

fn pid(id: &str) -> PluginId {
    PluginId::new(id).unwrap()
}

struct ProbePlugin {
    manifest: PluginManifest,
    trace: Trace,
    fail: bool,
}

impl Plugin for ProbePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let id = self.manifest.id.to_string();
        self.trace.lock().unwrap().push(format!("{id}:start"));
        let trace = self.trace.clone();
        let fail = self.fail;
        Box::pin(async move {
            ctx.state_set("checkpoint", b"started".to_vec())?;
            ctx.cleanup(cleanup(move || async move {
                trace.lock().unwrap().push(format!("{id}:cleanup"));
                Ok(())
            }))?;
            if fail {
                Err(PluginError::Lifecycle("测试插件启动失败".into()))
            } else {
                Ok(None)
            }
        })
    }
}

struct Fixture {
    kernel: Kernel,
    state: Arc<MemoryStateStore>,
    trace: Trace,
}

impl Fixture {
    fn new() -> Self {
        let state = Arc::new(MemoryStateStore::default());
        Self {
            kernel: Kernel::with_services(KernelServices {
                state: state.clone(),
                ..KernelServices::default()
            }),
            state,
            trace: Arc::default(),
        }
    }

    fn register(&self, id: &str, dependencies: &[(&str, &str)], fail: bool) {
        let mut manifest = PluginManifest::new(id, "1.0.0").unwrap();
        manifest.dependencies = dependencies
            .iter()
            .map(|(id, requirement)| PluginDependency {
                id: pid(id),
                requirement: Some((*requirement).into()),
            })
            .collect();
        self.kernel
            .register(Box::new(ProbePlugin {
                manifest,
                trace: self.trace.clone(),
                fail,
            }))
            .unwrap();
    }

    fn events(&self) -> Vec<String> {
        self.trace.lock().unwrap().clone()
    }

    fn states(&self) -> Vec<(PluginId, PluginState)> {
        self.kernel
            .plugins()
            .unwrap()
            .into_iter()
            .map(|plugin| (plugin.info.id, plugin.state))
            .collect()
    }

    fn assert_pristine(&self) {
        assert_eq!(self.events(), Vec::<String>::new(), "预检失败不得调用插件");
        for (id, state) in self.states() {
            assert_eq!(state, PluginState::Registered, "插件 {id} 状态被改变");
            assert_eq!(self.state.get(&id, "checkpoint").unwrap(), None);
        }
    }
}

fn assert_version_mismatch(error: PluginError, consumer: &str, provider: &str) {
    assert!(matches!(error, PluginError::DependencyVersionMismatch {
        plugin,
        dependency,
        requirement,
        found,
    } if plugin == pid(consumer)
        && dependency == pid(provider)
        && requirement == "^2"
        && found.as_str() == "1.0.0"));
}

#[tokio::test]
async fn later_sibling_version_mismatch_prevents_all_start_callbacks() {
    let fixture = Fixture::new();
    fixture.register("good", &[], false);
    fixture.register("bad", &[], false);
    fixture.register("consumer", &[("good", "^1"), ("bad", "^2")], false);

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert_version_mismatch(error, "consumer", "bad");
    fixture.assert_pristine();
}

#[tokio::test]
async fn transitive_version_mismatch_prevents_earlier_sibling_startup() {
    let fixture = Fixture::new();
    fixture.register("good", &[], false);
    fixture.register("bad", &[], false);
    fixture.register("middle", &[("bad", "^2")], false);
    fixture.register("consumer", &[("good", "^1"), ("middle", "^1")], false);

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert_version_mismatch(error, "middle", "bad");
    fixture.assert_pristine();
}

#[tokio::test]
async fn start_all_checks_every_root_before_starting_the_first_root() {
    let fixture = Fixture::new();
    fixture.register("a-independent", &[], false);
    fixture.register("provider", &[], false);
    fixture.register("z-consumer", &[("provider", "^2")], false);

    let error = fixture.kernel.start_all().await.unwrap_err();

    assert_version_mismatch(error, "z-consumer", "provider");
    fixture.assert_pristine();
}

#[tokio::test]
async fn missing_or_cyclic_later_branch_does_not_start_an_earlier_branch() {
    for cyclic in [false, true] {
        let fixture = Fixture::new();
        fixture.register("good", &[], false);
        fixture.register("consumer", &[("good", "^1"), ("middle", "^1")], false);
        let target = if cyclic { "consumer" } else { "missing" };
        fixture.register("middle", &[(target, "^1")], false);

        let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

        if cyclic {
            assert!(matches!(error, PluginError::DependencyCycle(id) if id == pid("consumer")));
        } else {
            assert!(
                matches!(error, PluginError::MissingDependency { plugin, dependency }
                if plugin == pid("middle") && dependency == pid("missing"))
            );
        }
        fixture.assert_pristine();
    }
}

#[tokio::test]
async fn rejection_preserves_registered_active_stopped_plugins_and_saved_bytes() {
    let fixture = Fixture::new();
    for id in ["active", "stopped", "registered", "bad"] {
        fixture.register(id, &[], false);
    }
    fixture.kernel.start(&pid("active")).await.unwrap();
    fixture.kernel.start(&pid("stopped")).await.unwrap();
    fixture.kernel.stop(&pid("stopped")).await.unwrap();
    fixture
        .state
        .set(&pid("stopped"), "checkpoint".into(), b"saved".to_vec())
        .unwrap();
    fixture.register(
        "consumer",
        &[
            ("active", "^1"),
            ("stopped", "^1"),
            ("registered", "^1"),
            ("bad", "^2"),
        ],
        false,
    );
    let previous_states = fixture.states();
    let previous_events = fixture.events();

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert_version_mismatch(error, "consumer", "bad");
    assert_eq!(fixture.events(), previous_events);
    assert_eq!(fixture.states(), previous_states);
    assert_eq!(
        fixture.state.get(&pid("stopped"), "checkpoint").unwrap(),
        Some(b"saved".to_vec())
    );
    assert_eq!(
        fixture.state.get(&pid("active"), "checkpoint").unwrap(),
        Some(b"started".to_vec())
    );
    assert_eq!(
        fixture.state.get(&pid("registered"), "checkpoint").unwrap(),
        None
    );
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn each_consumer_checks_its_own_requirement_for_a_shared_dependency() {
    let fixture = Fixture::new();
    fixture.register("shared", &[], false);
    fixture.register("left", &[("shared", "^1")], false);
    fixture.register("right", &[("shared", "^2")], false);
    fixture.register("consumer", &[("left", "^1"), ("right", "^1")], false);

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert_version_mismatch(error, "right", "shared");
    fixture.assert_pristine();
}

#[tokio::test]
async fn active_provider_still_must_satisfy_the_new_consumers_requirement() {
    let fixture = Fixture::new();
    fixture.register("provider", &[], false);
    fixture.kernel.start(&pid("provider")).await.unwrap();
    fixture.register("good", &[], false);
    fixture.register("consumer", &[("good", "^1"), ("provider", "^2")], false);
    let previous_events = fixture.events();
    let previous_states = fixture.states();

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert_version_mismatch(error, "consumer", "provider");
    assert_eq!(fixture.events(), previous_events);
    assert_eq!(fixture.states(), previous_states);
    assert_eq!(fixture.state.get(&pid("good"), "checkpoint").unwrap(), None);
    assert_eq!(
        fixture.state.get(&pid("provider"), "checkpoint").unwrap(),
        Some(b"started".to_vec())
    );
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn valid_diamond_starts_each_dependency_once_in_dependency_order() {
    let fixture = Fixture::new();
    fixture.register("shared", &[], false);
    fixture.register("left", &[("shared", "^1")], false);
    fixture.register("right", &[("shared", ">=1, <2")], false);
    fixture.register("consumer", &[("left", "^1"), ("right", "^1")], false);

    fixture.kernel.start(&pid("consumer")).await.unwrap();

    assert_eq!(
        fixture.events(),
        [
            "shared:start",
            "left:start",
            "right:start",
            "consumer:start"
        ]
    );
    assert!(
        fixture
            .states()
            .iter()
            .all(|(_, state)| *state == PluginState::Active)
    );
    fixture.kernel.stop_all().await.unwrap();
}

#[tokio::test]
async fn plugin_start_failure_after_preflight_still_cleans_up_and_rolls_back() {
    let fixture = Fixture::new();
    fixture.register("provider", &[], false);
    fixture.register("consumer", &[("provider", "^1")], true);

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert!(
        matches!(error, PluginError::PluginFailed { plugin, message }
        if plugin == pid("consumer") && message.contains("测试插件启动失败"))
    );
    assert_eq!(
        fixture.events(),
        [
            "provider:start",
            "consumer:start",
            "consumer:cleanup",
            "provider:cleanup",
        ]
    );
    assert_eq!(
        fixture.kernel.state(&pid("consumer")),
        Some(PluginState::Failed)
    );
    assert_eq!(
        fixture.kernel.state(&pid("provider")),
        Some(PluginState::Stopped)
    );
    // 真正执行过的插件仍遵循原契约：回滚不撤销已提交的状态字节。
    for id in ["consumer", "provider"] {
        assert_eq!(
            fixture.state.get(&pid(id), "checkpoint").unwrap(),
            Some(b"started".to_vec())
        );
    }
}

#[tokio::test]
async fn failed_dependency_is_rejected_before_starting_healthy_siblings() {
    let fixture = Fixture::new();
    fixture.register("failed", &[], true);
    fixture.kernel.start(&pid("failed")).await.unwrap_err();
    fixture.register("good", &[], false);
    fixture.register("consumer", &[("good", "^1"), ("failed", "^1")], false);
    let previous_events = fixture.events();
    let previous_states = fixture.states();

    let error = fixture.kernel.start(&pid("consumer")).await.unwrap_err();

    assert!(matches!(error, PluginError::InvalidLifecycle { plugin, .. }
        if plugin == pid("failed")));
    assert_eq!(fixture.events(), previous_events);
    assert_eq!(fixture.states(), previous_states);
}

#[tokio::test]
async fn starting_one_plugin_does_not_validate_unrelated_plugins() {
    let fixture = Fixture::new();
    fixture.register("good", &[], false);
    fixture.register("unrelated", &[("missing", "^1")], false);

    fixture.kernel.start(&pid("good")).await.unwrap();

    assert_eq!(fixture.events(), ["good:start"]);
    assert_eq!(
        fixture.kernel.state(&pid("unrelated")),
        Some(PluginState::Registered)
    );
    fixture.kernel.stop_all().await.unwrap();
}
