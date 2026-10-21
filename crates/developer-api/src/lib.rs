//! 开发能力公开契约。宿主授权、路径及凭据引用不能由模型参数覆盖。
use eve_llm_api::{ToolExecutionContext, ToolFuture};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

mod upgrade;
pub use upgrade::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeveloperConfig {
    pub workspace: PathBuf,
    /// 独立持久账本/候选目录；None 时由宿主使用状态目录下的 developer-tools。
    #[serde(default)]
    pub storage_directory: Option<PathBuf>,
    pub operators: BTreeSet<String>,
    /// 显式开放开发后端的普通访问；QQ 宿主仍独立执行主人/管理员管理策略。
    #[serde(default = "ordinary_access")]
    pub ordinary_access: bool,
    #[serde(default)]
    pub commands: bool,
    #[serde(default)]
    pub github: bool,
    /// 向所有具有宿主绑定身份的聊天用户开放 GitHub 工具；不开放文件/命令/升级工具。
    #[serde(default)]
    pub github_all_users: bool,
    /// GitHub 写入不再使用宿主仓库白名单，仍受 token 的实际授权约束。
    #[serde(default)]
    pub github_all_repositories: bool,
    #[serde(default)]
    pub github_write_repositories: BTreeSet<String>,
    #[serde(default = "token_env")]
    pub github_token_env: String,
    #[serde(default = "timeout")]
    pub command_timeout_ms: u64,
    #[serde(default = "rounds")]
    /// 0 表示不限制工具批次数；正数为显式上限。
    pub max_tool_rounds: usize,
    #[serde(default)]
    pub self_upgrade: bool,
    /// 独立监督器的可信请求目录；未配置时只准备候选，不注册实时切换工具。
    #[serde(default)]
    pub live_upgrade: Option<PathBuf>,
    #[serde(default = "cargo")]
    pub cargo_program: String,
    #[serde(default)]
    pub cargo_arguments: Vec<String>,
}
fn token_env() -> String {
    "EVE_GITHUB_TOKEN".into()
}
fn ordinary_access() -> bool {
    false
}

/// 宿主提供的平台身份授权；模型参数、昵称及自称不能实现此绑定。
pub trait DeveloperAuthorization: Send + Sync {
    fn is_operator(&self, scope: &eve_llm_api::ContextScope) -> bool;
}
fn timeout() -> u64 {
    600_000
}
fn rounds() -> usize {
    8
}
fn cargo() -> String {
    "cargo".into()
}

impl DeveloperConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.workspace.is_absolute()
            || self
                .storage_directory
                .as_ref()
                .is_some_and(|p| !p.is_absolute())
            || self.operators.is_empty()
            || self.operators.iter().any(|s| {
                s.is_empty() || s.len() > 256 || s.trim() != s || s.chars().any(char::is_control)
            })
            || !(1..=600_000).contains(&self.command_timeout_ms)
            || !matches!(
                self.github_token_env.as_str(),
                "EVE_GITHUB_TOKEN" | "GH_TOKEN" | "GITHUB_TOKEN"
            )
            || (self.self_upgrade && !self.commands)
            || self
                .live_upgrade
                .as_ref()
                .is_some_and(|p| !p.is_absolute() || !self.self_upgrade || !self.commands)
            || self.cargo_program.trim().is_empty()
            || self.cargo_arguments.len() > 8
            || self
                .cargo_arguments
                .iter()
                .any(|s| s.len() > 256 || s.chars().any(char::is_control))
            || self
                .github_write_repositories
                .iter()
                .any(|s| !valid_repository(s))
        {
            return Err("开发工具配置无效；需要绝对工作目录、明确操作者与有效期限/凭据引用");
        }
        Ok(())
    }
}
pub fn valid_repository(value: &str) -> bool {
    let parts: Vec<_> = value.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|s| {
            !s.is_empty()
                && s.len() <= 100
                && !matches!(*s, "." | "..")
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileArguments {
    pub path: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub content: Option<String>,
    /// 写已有文件必须使用上次读取的 SHA；空串表示仅创建新文件。
    #[serde(default)]
    pub expected_sha256: Option<String>,
}
/// 精确替换：old_string 必须在文件中恰好出现一次；expected_sha256 为上次读取的文件 SHA。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditArguments {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    pub expected_sha256: String,
}
/// 字面文本搜索；path 为工作区相对目录，默认根目录。
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SearchArguments {
    #[serde(default = "dot")]
    pub path: String,
    pub query: String,
    #[serde(default = "search_limit")]
    pub max_results: usize,
}
fn search_limit() -> usize {
    50
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArguments {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "dot")]
    pub cwd: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}
fn dot() -> String {
    ".".into()
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GithubArguments {
    pub method: String,
    pub repository: String,
    /// 相对仓库 REST 路径；搜索为 search/code、search/issues、search/commits、search/repositories。
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<Value>,
}

impl GithubArguments {
    /// 将同仓库 REST 前缀及 GET contents 的 query.path 归一化；不扩展权限。
    /// Tool 适配器与直接使用后端的调用方均应采用此契约。
    pub fn normalized(mut self) -> Result<Self, &'static str> {
        if !valid_repository(&self.repository)
            || !["GET", "POST", "PATCH", "PUT", "DELETE"].contains(&self.method.as_str())
            || !valid_github_resource(&self.resource)
            || self.query.len() > 24
            || self.query.iter().any(|(k, v)| {
                k.len() > 64
                    || v.len() > 8192
                    || k.chars().any(char::is_control)
                    || v.chars().any(char::is_control)
            })
            || self
                .body
                .as_ref()
                .is_some_and(|v| v.to_string().len() > 256 * 1024)
            || (self.method == "GET" && self.body.is_some())
        {
            return Err(
                "GitHub 参数无效：repository 使用 owner/repo，resource 使用 contents/文件路径等相对资源；不能提供 URL 或路径跳转",
            );
        }
        if let Some(rest) = self.resource.strip_prefix("repos/") {
            let mut parts = rest.splitn(3, '/');
            let repository = format!(
                "{}/{}",
                parts.next().unwrap_or(""),
                parts.next().unwrap_or("")
            );
            if !repository.eq_ignore_ascii_case(&self.repository) {
                return Err("GitHub resource 的仓库前缀与 repository 不一致；不会请求其他仓库");
            }
            self.resource = parts.next().unwrap_or("").to_owned();
            if !valid_github_resource(&self.resource) || self.resource.starts_with("repos/") {
                return Err(
                    "GitHub resource 只能包含一次同仓库前缀，后面须为 contents/文件路径等相对资源",
                );
            }
        }
        if let Some(path) = self.query.remove("path") {
            if self.method != "GET"
                || self.resource != "contents"
                || path.is_empty()
                || !valid_github_resource(&path)
            {
                return Err(
                    "GitHub query.path 仅用于 GET contents，且不能与 resource 中的文件路径重复；请传 resource=contents/文件路径，query 只传 ref 等参数",
                );
            }
            self.resource = format!("contents/{path}");
            if !valid_github_resource(&self.resource) {
                return Err("GitHub contents 文件路径超过上限");
            }
        }
        Ok(self)
    }
}

fn valid_github_resource(value: &str) -> bool {
    value.len() <= 4096
        && !value.starts_with('/')
        && !value.contains(['?', '#', '%', '\\', ':'])
        && !value.chars().any(char::is_control)
        && !value.split('/').any(|p| matches!(p, "." | ".."))
}

#[derive(Clone, Debug)]
pub enum DeveloperOperation {
    List(FileArguments),
    Read(FileArguments),
    Write(FileArguments),
    Edit(EditArguments),
    Search(SearchArguments),
    Command(CommandArguments),
    Github(GithubArguments),
    PrepareUpgrade,
    ApplyUpgrade(ApplyUpgradeArguments),
    UpgradeStatus,
}
/// 可替换后端；接收宿主的可信会话身份与取消状态，而非模型自报 owner。
pub trait DeveloperBackend: Send + Sync {
    fn execute(
        &self,
        operation: DeveloperOperation,
        context: ToolExecutionContext,
    ) -> ToolFuture<'_>;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn github_routes_normalize_without_changing_repository_or_hiding_conflicts() {
        let base = GithubArguments {
            method: "GET".into(),
            repository: "Anuken/Mindustry".into(),
            resource: "repos/Anuken/Mindustry/contents".into(),
            query: BTreeMap::from([
                ("path".into(), "core/src/MendProjector.java".into()),
                ("ref".into(), "master".into()),
            ]),
            body: None,
        };
        let normalized = base.clone().normalized().unwrap();
        assert_eq!(normalized.resource, "contents/core/src/MendProjector.java");
        assert_eq!(
            normalized.query,
            BTreeMap::from([("ref".into(), "master".into())])
        );
        assert_eq!(
            normalized.clone().normalized().unwrap().resource,
            normalized.resource
        );
        for resource in [
            "repos/foreign/repo/contents",
            "repos/Anuken/Mindustry/repos/Anuken/Mindustry/contents",
            "repos/Anuken/Mindustry//contents",
            "contents/a",
            "https://api.github.com/repos/Anuken/Mindustry/contents",
            "/contents",
            "contents/../issues",
            "contents/%2e%2e/issues",
            "contents/a?ref=main",
        ] {
            let mut a = base.clone();
            a.resource = resource.into();
            assert!(a.normalized().is_err(), "{resource}");
        }
        for path in [
            "../issues",
            "/issues",
            "a/../../issues",
            "%2e%2e/issues",
            "https://other.invalid/",
        ] {
            let mut a = base.clone();
            a.query.insert("path".into(), path.into());
            assert!(a.normalized().is_err(), "{path}");
        }
        let mut write = base.clone();
        write.method = "PUT".into();
        assert!(write.normalized().is_err());
        let mut info = base;
        info.query.clear();
        info.resource = "repos/anuken/mindustry".into();
        assert_eq!(info.normalized().unwrap().resource, "");
    }
    #[test]
    fn host_configuration_accepts_separate_durable_storage_and_rejects_other_secret_references() {
        let base =
            serde_json::json!({"workspace":std::env::current_dir().unwrap(),"operators":["owner"]});
        let mut config: DeveloperConfig = serde_json::from_value(base).unwrap();
        assert!(config.storage_directory.is_none());
        config.validate().unwrap();
        for rounds in [0, 8, 32, 64] {
            config.max_tool_rounds = rounds;
            config.validate().unwrap();
        }
        config.storage_directory = Some(std::env::temp_dir());
        config.validate().unwrap();
        config.storage_directory = Some("relative".into());
        assert!(config.validate().is_err());
        config.storage_directory = None;
        config.github_token_env = "QQBOT_APP_SECRET".into();
        assert!(config.validate().is_err());
    }
}
