use eve_kernel::Kernel;
use eve_plugin_api::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn pid(id: &str) -> PluginId {
    PluginId::new(id).unwrap()
}

struct CountedPlugin {
    manifest: PluginManifest,
    starts: Arc<AtomicUsize>,
    fail_on_restart: bool,
}

impl Plugin for CountedPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(&mut self, _: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        let previous_starts = self.starts.fetch_add(1, Ordering::SeqCst);
        let fail = self.fail_on_restart && previous_starts > 0;
        Box::pin(async move {
            if fail {
                Err(PluginError::Lifecycle("叶子插件拒绝再次启动".into()))
            } else {
                Ok(None)
            }
        })
    }
}

fn register(
    kernel: &Kernel,
    id: &str,
    dependencies: &[&str],
    fail_on_restart: bool,
) -> Arc<AtomicUsize> {
    let starts = Arc::new(AtomicUsize::new(0));
    let mut manifest = PluginManifest::new(id, "0.1.0").unwrap();
    manifest.dependencies = dependencies
        .iter()
        .map(|id| PluginDependency {
            id: pid(id),
            requirement: None,
        })
        .collect();
    kernel
        .register(Box::new(CountedPlugin {
            manifest,
            starts: starts.clone(),
            fail_on_restart,
        }))
        .unwrap();
    starts
}

async fn verify_nested_restart_failure(start_all: bool) {
    let kernel = Kernel::new();
    // ID排序让start_all从消费者开始，经过两层依赖后触发叶子失败。
    let consumer_starts = register(&kernel, "a-consumer", &["b-middle"], false);
    let middle_starts = register(&kernel, "b-middle", &["z-leaf"], false);
    let leaf_starts = register(&kernel, "z-leaf", &[], true);
    kernel.start(&pid("a-consumer")).await.unwrap();
    kernel.stop_all().await.unwrap();
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|plugin| plugin.state == PluginState::Stopped)
    );

    let error = if start_all {
        kernel.start_all().await.unwrap_err()
    } else {
        kernel.start(&pid("a-consumer")).await.unwrap_err()
    };
    assert!(error.to_string().contains("叶子插件拒绝再次启动"));
    assert_eq!(consumer_starts.load(Ordering::SeqCst), 1);
    assert_eq!(middle_starts.load(Ordering::SeqCst), 1);
    assert_eq!(leaf_starts.load(Ordering::SeqCst), 2);
    assert_eq!(kernel.state(&pid("z-leaf")), Some(PluginState::Failed));
    for id in ["a-consumer", "b-middle"] {
        assert_eq!(
            kernel.state(&pid(id)),
            Some(PluginState::Stopped),
            "依赖失败不能改变未重新启动的插件原状态：{id}"
        );
    }
    assert!(
        kernel
            .plugins()
            .unwrap()
            .iter()
            .all(|plugin| plugin.state != PluginState::WaitingDependencies)
    );
}

#[tokio::test]
async fn start_preserves_stopped_consumers_when_nested_dependency_restart_fails() {
    verify_nested_restart_failure(false).await;
}

#[tokio::test]
async fn start_all_preserves_stopped_consumers_when_nested_dependency_restart_fails() {
    verify_nested_restart_failure(true).await;
}

#[tokio::test]
async fn first_start_with_missing_or_cyclic_dependencies_restores_registered_state() {
    for cyclic in [false, true] {
        let kernel = Kernel::new();
        let first = register(&kernel, "a-consumer", &["b-provider"], false);
        let second = register(&kernel, "b-provider", &["c-provider"], false);
        let third = if cyclic {
            register(&kernel, "c-provider", &["a-consumer"], false)
        } else {
            register(&kernel, "c-provider", &["missing"], false)
        };
        for start_all in [false, true] {
            let error = if start_all {
                kernel.start_all().await.unwrap_err()
            } else {
                kernel.start(&pid("a-consumer")).await.unwrap_err()
            };
            if cyclic {
                assert!(matches!(error, PluginError::DependencyCycle(_)));
            } else {
                assert!(
                    matches!(error, PluginError::MissingDependency { dependency, .. } if dependency == pid("missing"))
                );
            }
            assert!(
                kernel
                    .plugins()
                    .unwrap()
                    .iter()
                    .all(|plugin| plugin.state == PluginState::Registered)
            );
            for starts in [&first, &second, &third] {
                assert_eq!(starts.load(Ordering::SeqCst), 0);
            }
        }
    }
}
