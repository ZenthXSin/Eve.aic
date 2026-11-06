//! 常见实际凭据与身份账本路径；普通源码、项目规则和配置不因关键词受到限制。
use std::path::{Component, Path};

/// 只识别约定的凭据位置；宿主另以 `DeveloperConfig::protected_paths` 绑定实际秘密路径。
pub fn credential_path(path: &str) -> bool {
    let path = Path::new(path);
    if [
        ".docker/config.json",
        ".kube/config",
        ".config/gh/hosts.yml",
        ".composer/auth.json",
        ".config/composer/auth.json",
    ]
    .iter()
    .any(|suffix| path.ends_with(suffix))
        || ["/run/secrets", "/var/run/secrets"]
            .iter()
            .any(|root| path.starts_with(root))
    {
        return true;
    }
    path.components().any(|part| match part {
        Component::Normal(name) => {
            let name = name.to_string_lossy().to_ascii_lowercase();
            name == ".env"
                || name.starts_with(".env.")
                || matches!(
                    name.as_str(),
                    "private"
                        | ".ssh"
                        | ".aws"
                        | ".gnupg"
                        | ".netrc"
                        | ".npmrc"
                        | ".pgpass"
                        | "id_rsa"
                        | "id_ed25519"
                        | "business-permissions.json"
                        | "business-permissions.initialized"
                        | "business-permissions.failed"
                        | "admin-bindings.json"
                )
                || (Path::new(&name)
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| {
                        matches!(
                            stem,
                            "secret"
                                | "secrets"
                                | "credential"
                                | "credentials"
                                | "client_secret"
                                | "service-account"
                                | "service_account"
                        )
                    })
                    && Path::new(&name)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| {
                            matches!(extension, "json" | "toml" | "yaml" | "yml" | "ini" | "txt")
                        }))
                || name.ends_with(".pem")
                || name.ends_with(".key")
                || name.ends_with(".p12")
        }
        _ => false,
    })
}
