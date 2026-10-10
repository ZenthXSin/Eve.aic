//! 开发能力公开契约。宿主授权、路径及凭据引用不能由模型参数覆盖。
use eve_llm_api::{ToolExecutionContext, ToolFuture};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeveloperConfig {
    pub workspace: PathBuf,
    pub operators: BTreeSet<String>,
    #[serde(default)]
    pub commands: bool,
    #[serde(default)]
    pub github: bool,
    #[serde(default)]
    pub github_write_repositories: BTreeSet<String>,
    #[serde(default = "token_env")]
    pub github_token_env: String,
    #[serde(default = "timeout")]
    pub command_timeout_ms: u64,
    #[serde(default = "rounds")]
    pub max_tool_rounds: usize,
    #[serde(default)]
    pub self_upgrade: bool,
    #[serde(default = "cargo")]
    pub cargo_program: String,
    #[serde(default)]
    pub cargo_arguments: Vec<String>,
}
fn token_env() -> String {
    "EVE_GITHUB_TOKEN".into()
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
            || self.operators.is_empty()
            || self.operators.iter().any(|s| {
                s.is_empty() || s.len() > 256 || s.trim() != s || s.chars().any(char::is_control)
            })
            || !(1..=600_000).contains(&self.command_timeout_ms)
            || !(1..=32).contains(&self.max_tool_rounds)
            || !matches!(
                self.github_token_env.as_str(),
                "EVE_GITHUB_TOKEN" | "GH_TOKEN" | "GITHUB_TOKEN"
            )
            || (self.self_upgrade && !self.commands)
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

#[derive(Clone, Debug)]
pub enum DeveloperOperation {
    List(FileArguments),
    Read(FileArguments),
    Write(FileArguments),
    Command(CommandArguments),
    Github(GithubArguments),
    PrepareUpgrade,
}
/// 可替换后端；接收宿主的可信会话身份与取消状态，而非模型自报 owner。
pub trait DeveloperBackend: Send + Sync {
    fn execute(
        &self,
        operation: DeveloperOperation,
        context: ToolExecutionContext,
    ) -> ToolFuture<'_>;
}
