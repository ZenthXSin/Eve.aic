use eve_plugin_api::{Permission, PermissionChecker, PluginError, PluginManifest, PluginResult};

/// 仅检查 Manifest 的精确权限声明，不拦截原生 Rust 的系统调用。
#[derive(Default)]
pub struct DeclaredPermissionChecker;

impl PermissionChecker for DeclaredPermissionChecker {
    fn check(&self, manifest: &PluginManifest, permission: &Permission) -> PluginResult<()> {
        if manifest.permissions.contains(permission) {
            Ok(())
        } else {
            Err(PluginError::PermissionDenied {
                plugin: manifest.id.clone(),
                permission: permission.clone(),
            })
        }
    }
}
