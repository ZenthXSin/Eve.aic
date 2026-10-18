//! 只读参考资料的公开契约。
//!
//! 参考资料是宿主装配的固定版本文本（例如某个发布版本的源码或随仓库提供的资料），
//! 对话中按名称、路径与行号查证；契约只提供搜索和按行读取，不提供写入、执行或联网。

use serde::Serialize;
use std::fmt;

/// 单次搜索最多返回的命中数。
pub const MAX_SEARCH_RESULTS: usize = 50;
/// 单次读取最多返回的行数。
pub const MAX_READ_LINES: usize = 200;
/// 搜索词的字节上限。
pub const MAX_QUERY_BYTES: usize = 200;

/// 一份已装配的资料。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReferenceRoot {
    /// 资料名，小写字母、数字和连字符，供工具参数引用。
    pub name: String,
    /// 资料来源的固定版本（例如 git 提交）；无法确定时为空。
    pub revision: Option<String>,
    /// 可检索的文件数。
    pub files: usize,
    /// 顶层目录，帮助模型缩小搜索范围。
    pub top_level: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchRequest {
    /// 不区分大小写的字面文本，同时匹配文件路径和行内容。
    pub query: String,
    /// 只搜索指定资料；为空时搜索全部。
    pub root: Option<String>,
    /// 只搜索以此开头的相对路径。
    pub path_prefix: Option<String>,
    pub max_results: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchHit {
    pub root: String,
    pub path: String,
    /// 1 起算的行号；路径命中时为 0。
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// 还有更多命中没有返回。
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadRequest {
    pub root: String,
    pub path: String,
    /// 1 起算的起始行。
    pub start_line: usize,
    pub max_lines: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Excerpt {
    pub root: String,
    pub path: String,
    pub revision: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub total_lines: usize,
    /// 每行带“行号: ”前缀的原文。
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReferenceError {
    /// 参数不符合契约上限或格式。
    Invalid(String),
    UnknownRoot(String),
    NotFound(String),
}

impl fmt::Display for ReferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "参数无效：{message}"),
            Self::UnknownRoot(name) => write!(f, "没有名为 {name} 的资料"),
            Self::NotFound(path) => write!(f, "资料中没有 {path}"),
        }
    }
}

impl std::error::Error for ReferenceError {}

/// 只读参考资料库。实现必须只返回装配时确定的内容，不跟随运行期新增的文件。
pub trait ReferenceLibrary: Send + Sync {
    fn roots(&self) -> Vec<ReferenceRoot>;
    fn search(&self, request: &SearchRequest) -> Result<SearchResult, ReferenceError>;
    fn read(&self, request: &ReadRequest) -> Result<Excerpt, ReferenceError>;
}

/// 资料名：1 至 32 个小写字母、数字或连字符，不以连字符开头。
pub fn valid_root_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// 相对路径：用 `/` 分隔，不含空段、`.`、`..`、反斜杠或控制字符。
pub fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl SearchRequest {
    pub fn validate(&self) -> Result<(), ReferenceError> {
        let query = self.query.trim();
        if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control)
        {
            return Err(ReferenceError::Invalid(format!(
                "query 须为 1 至 {MAX_QUERY_BYTES} 字节的单行文本"
            )));
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&self.max_results) {
            return Err(ReferenceError::Invalid(format!(
                "max_results 须在 1 至 {MAX_SEARCH_RESULTS} 之间"
            )));
        }
        if self
            .root
            .as_deref()
            .is_some_and(|root| !valid_root_name(root))
        {
            return Err(ReferenceError::Invalid("root 格式无效".into()));
        }
        if self
            .path_prefix
            .as_deref()
            .is_some_and(|prefix| !valid_relative_path(prefix.trim_end_matches('/')))
        {
            return Err(ReferenceError::Invalid(
                "path_prefix 须为资料内相对路径".into(),
            ));
        }
        Ok(())
    }
}

impl ReadRequest {
    pub fn validate(&self) -> Result<(), ReferenceError> {
        if !valid_root_name(&self.root) {
            return Err(ReferenceError::Invalid("root 格式无效".into()));
        }
        if !valid_relative_path(&self.path) {
            return Err(ReferenceError::Invalid("path 须为资料内相对路径".into()));
        }
        if self.start_line == 0 || !(1..=MAX_READ_LINES).contains(&self.max_lines) {
            return Err(ReferenceError::Invalid(format!(
                "start_line 从 1 起算，max_lines 须在 1 至 {MAX_READ_LINES} 之间"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths_reject_escapes() {
        assert!(valid_root_name("mindustry") && valid_root_name("eve-portable"));
        for bad in ["", "-a", "Mindustry", "a_b", &"a".repeat(33)] {
            assert!(!valid_root_name(bad), "{bad}");
        }
        assert!(valid_relative_path("core/src/mindustry/Vars.java"));
        for bad in ["", "/etc/passwd", "a/../b", "a//b", "./a", "a\\b", "a\nb"] {
            assert!(!valid_relative_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn requests_enforce_limits() {
        let search = |query: &str, max_results| SearchRequest {
            query: query.into(),
            root: None,
            path_prefix: None,
            max_results,
        };
        assert!(search("MendProjector", 20).validate().is_ok());
        assert!(search("  ", 20).validate().is_err());
        assert!(search("a", 0).validate().is_err());
        assert!(search("a", MAX_SEARCH_RESULTS + 1).validate().is_err());
        assert!(
            search(&"x".repeat(MAX_QUERY_BYTES + 1), 5)
                .validate()
                .is_err()
        );
        let prefixed = SearchRequest {
            path_prefix: Some("core/src/".into()),
            ..search("a", 5)
        };
        assert!(prefixed.validate().is_ok());
        let read = |start_line, max_lines| ReadRequest {
            root: "mindustry".into(),
            path: "core/src/A.java".into(),
            start_line,
            max_lines,
        };
        assert!(read(1, 120).validate().is_ok());
        assert!(read(0, 10).validate().is_err());
        assert!(read(1, MAX_READ_LINES + 1).validate().is_err());
    }
}
