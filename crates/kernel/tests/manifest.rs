use eve_kernel::Kernel;
use eve_plugin_api::{
    Plugin, PluginDependency, PluginError, PluginFuture, PluginId, PluginManifest, PluginResult,
    PluginState,
};

struct TestPlugin {
    manifest: PluginManifest,
}

impl Plugin for TestPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    fn start(
        &mut self,
        _ctx: eve_plugin_api::PluginContext,
    ) -> PluginFuture<'_, Option<eve_plugin_api::Cleanup>> {
        Box::pin(async { Ok(None) })
    }
}

fn manifest(id: &str, version: &str) -> PluginManifest {
    PluginManifest::new(id, version).unwrap()
}

fn dependency(id: &str, requirement: Option<&str>) -> PluginDependency {
    PluginDependency {
        id: PluginId::new(id).unwrap(),
        requirement: requirement.map(str::to_owned),
    }
}

fn register(kernel: &Kernel, manifest: PluginManifest) -> PluginResult<()> {
    kernel.register(Box::new(TestPlugin { manifest }))
}

#[test]
fn manifest_version_must_be_semver() {
    let error = PluginManifest::new("demo", "development").unwrap_err();
    assert!(matches!(error, PluginError::InvalidManifest(message) if message.contains("SemVer")));
}

#[test]
fn registration_rejects_invalid_dependency_declarations() {
    let kernel = Kernel::new();

    let mut duplicate = manifest("duplicate", "1.0.0");
    duplicate.dependencies = vec![dependency("provider", None), dependency("provider", None)];
    let error = register(&kernel, duplicate).unwrap_err();
    assert!(
        matches!(error, PluginError::InvalidManifest(message) if message.contains("more than once"))
    );

    let mut self_dependency = manifest("self", "1.0.0");
    self_dependency.dependencies = vec![dependency("self", None)];
    let error = register(&kernel, self_dependency).unwrap_err();
    assert!(matches!(error, PluginError::InvalidManifest(message) if message.contains("itself")));

    let mut invalid_requirement = manifest("invalid-requirement", "1.0.0");
    invalid_requirement.dependencies = vec![dependency("provider", Some("not-semver"))];
    let error = register(&kernel, invalid_requirement).unwrap_err();
    assert!(
        matches!(error, PluginError::InvalidManifest(message) if message.contains("invalid version requirement"))
    );
}

#[tokio::test]
async fn dependency_requirement_is_checked_before_starting_either_plugin() {
    let kernel = Kernel::new();
    register(&kernel, manifest("provider", "1.2.3")).unwrap();
    let mut consumer = manifest("consumer", "1.0.0");
    consumer.dependencies = vec![dependency("provider", Some("^2.0"))];
    register(&kernel, consumer).unwrap();

    let consumer_id = PluginId::new("consumer").unwrap();
    let error = kernel.start(&consumer_id).await.unwrap_err();
    assert!(matches!(error, PluginError::DependencyVersionMismatch {
        plugin,
        dependency,
        requirement,
        found,
    } if plugin == consumer_id
        && dependency == PluginId::new("provider").unwrap()
        && requirement == "^2.0"
        && found.as_str() == "1.2.3"));
    assert_eq!(kernel.state(&consumer_id), Some(PluginState::Registered));
    assert_eq!(
        kernel.state(&PluginId::new("provider").unwrap()),
        Some(PluginState::Registered)
    );
}

#[tokio::test]
async fn compatible_dependency_requirement_allows_startup() {
    let kernel = Kernel::new();
    register(&kernel, manifest("provider", "1.2.3")).unwrap();
    let mut consumer = manifest("consumer", "1.0.0");
    consumer.dependencies = vec![dependency("provider", Some(">=1.0, <2.0"))];
    register(&kernel, consumer).unwrap();

    let consumer_id = PluginId::new("consumer").unwrap();
    kernel.start(&consumer_id).await.unwrap();
    assert_eq!(kernel.state(&consumer_id), Some(PluginState::Active));
    assert_eq!(
        kernel.state(&PluginId::new("provider").unwrap()),
        Some(PluginState::Active)
    );
}
