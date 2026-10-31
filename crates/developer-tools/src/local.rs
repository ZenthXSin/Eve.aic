use eve_developer_api::*;
use eve_llm_api::{ToolExecutionContext, ToolExecutionError, ToolFuture};
use reqwest::{Client, Url, redirect::Policy};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
};
mod live_upgrade;
mod member;

const MAX_FILE: usize = 8 * 1024 * 1024;
const MAX_OUTPUT: usize = 32 * 1024;
const MAX_SEARCH_FILES: usize = 20_000;
const MAX_SEARCH_LINE: usize = 240;
/// 搜索跳过的目录名：版本库、工具账本与构建产物，不属于源码阅读范围。
const SKIPPED_NAMES: [&str; 3] = [".git", ".eve-developer", "target"];
fn sensitive_path(path: &str) -> bool {
    Path::new(path).components().any(|c| match c {
        Component::Normal(s) => {
            let s = s.to_string_lossy().to_ascii_lowercase();
            s.starts_with('.')
                || matches!(
                    s.as_str(),
                    "agent.md" | "agents.md" | "cargo.toml" | "cargo.lock"
                )
                || s.contains("secret")
                || s.contains("credential")
                || s.contains("config")
                || s.ends_with(".pem")
                || s.ends_with(".key")
        }
        Component::CurDir => false,
        _ => true,
    })
}
fn stopped(context: &ToolExecutionContext) -> bool {
    context.is_cancel_requested()
        || context
            .deadline()
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
}
fn failed(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Failed(message.into())
}
fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// 统计模式的出现次数（含重叠），只数到 2 即可判断是否唯一。
fn occurrences(text: &str, pattern: &str) -> usize {
    if pattern.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(offset) = text[start..].find(pattern) {
        count += 1;
        if count > 1 {
            break;
        }
        let at = start + offset;
        start = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    count
}
fn shorten(text: &str, limit: usize) -> (&str, bool) {
    let mut n = text.len().min(limit);
    while !text.is_char_boundary(n) {
        n -= 1;
    }
    (&text[..n], n < text.len())
}

/// 命令为受信操作者的宿主执行，工作目录限制不是 OS 沙箱。密钥仅存在 HTTP 客户端侧。
pub struct LocalDeveloper {
    config: DeveloperConfig,
    client: Client,
    token: Option<String>,
    root: PathBuf,
    journal: PathBuf,
    next: AtomicU64,
    serial: Mutex<()>,
    /// 只由测试构造替换；生产始终为 https://api.github.com/。
    api: Url,
    authorization: Option<std::sync::Arc<dyn DeveloperAuthorization>>,
    member_runtime: Option<MemberRuntime>,
    isolated: bool,
    command_policy: Option<std::sync::Arc<dyn eve_developer_api::CommandPolicy>>,
    action_reviewer: Option<std::sync::Arc<dyn ActionReviewer>>,
}
/// 交给审查者的用户原话条数与行动轨迹条数；用户原话正文不裁剪，
/// 工具轨迹与命令摘要仍保持有界。
const REVIEW_USER_MESSAGES: usize = 6;
const REVIEW_RECENT_ACTIONS: usize = 8;
const REVIEW_COMMAND_CHARS: usize = 2000;
const REVIEW_ACTION_CHARS: usize = 300;
fn clip(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}
/// 从本轮上下文取审查依据。宿主标记的委托执行没有人在场，用户角色的消息是派发时
/// 由模型写的任务说明，不作为用户原话交给审查者。
fn review_request(
    a: &CommandArguments,
    reason: String,
    context: &ToolExecutionContext,
) -> Result<ActionReviewRequest, ToolExecutionError> {
    let delegated = context.is_delegated();
    let messages = context.conversation().unwrap_or_default();
    let user_messages = if delegated {
        vec![]
    } else if let Some(originals) = context.review_user_messages() {
        originals
            .iter()
            .rev()
            .take(REVIEW_USER_MESSAGES)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        let mut texts: Vec<String> = messages
            .iter()
            .rev()
            .filter(|m| m.role == eve_llm_api::ChatRole::User)
            .filter_map(|m| m.text.as_deref())
            .filter(|t| !t.trim().is_empty())
            .take(REVIEW_USER_MESSAGES)
            .map(ToOwned::to_owned)
            .collect();
        texts.reverse();
        texts
    };
    let mut recent_actions: Vec<String> = messages
        .iter()
        .rev()
        .flat_map(|m| m.tool_calls.iter().rev())
        .take(REVIEW_RECENT_ACTIONS)
        .map(|c| clip(&format!("{} {}", c.name, c.arguments), REVIEW_ACTION_CHARS))
        .collect();
    recent_actions.reverse();
    let mut action = vec![a.program.clone()];
    action.extend(a.args.iter().cloned());
    let request = ActionReviewRequest {
        program: a.program.clone(),
        arguments: a.args.clone(),
        workspace: String::new(),
        isolated: false,
        action: clip(
            &shlex::try_join(action.iter().map(String::as_str))
                .unwrap_or_else(|_| action.join(" ")),
            REVIEW_COMMAND_CHARS,
        ),
        cwd: a.cwd.clone(),
        policy_reason: reason,
        claimed_confirmation: a.owner_confirmed,
        delegated,
        user_messages,
        recent_actions,
    };
    Ok(request)
}
impl LocalDeveloper {
    pub fn new(
        config: DeveloperConfig,
        journal: PathBuf,
        token: Option<String>,
    ) -> Result<Self, ToolExecutionError> {
        config.validate().map_err(failed)?;
        let root =
            fs::canonicalize(&config.workspace).map_err(|_| failed("开发源码工作区不可读取"))?;
        if !root.is_dir() {
            return Err(failed("开发源码工作区必须是目录"));
        }
        fs::create_dir_all(&journal).map_err(|_| failed("开发工具账本无法创建"))?;
        let next = match fs::read(journal.join("operations.jsonl")) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| failed("开发工具账本损坏；保留原文件"))?;
                if !text.is_empty() && !text.ends_with('\n') {
                    return Err(failed("开发工具账本存在不完整提交；保留原文件"));
                }
                let mut max = 0;
                for line in text.lines() {
                    let v: Value = serde_json::from_str(line)
                        .map_err(|_| failed("开发工具账本损坏；保留原文件"))?;
                    let id = v["id"]
                        .as_u64()
                        .ok_or_else(|| failed("开发工具账本版本无效"))?;
                    max = max.max(id);
                }
                max.checked_add(1)
                    .ok_or_else(|| failed("开发工具账本已满"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 1,
            Err(_) => return Err(failed("开发工具账本无法读取")),
        };
        match fs::read(journal.join("action-reviews.jsonl")) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes)
                    .map_err(|_| failed("行动审核审计损坏；保留原文件"))?;
                if !text.is_empty() && !text.ends_with('\n') {
                    return Err(failed("行动审核审计存在不完整提交；保留原文件"));
                }
                for line in text.lines() {
                    let entry: Value = serde_json::from_str(line)
                        .map_err(|_| failed("行动审核审计损坏；保留原文件"))?;
                    if entry["format_version"] != 1
                        || !entry["request_sha256"].as_str().is_some_and(|s| {
                            s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                        || !matches!(entry["status"].as_str(), Some("failed" | "reviewed"))
                        || !entry["allowed"].is_boolean()
                    {
                        return Err(failed("行动审核审计版本或记录无效；保留原文件"));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failed("行动审核审计无法读取；保留原文件")),
        }
        let token = token.filter(|v| !v.trim().is_empty());
        if token
            .as_ref()
            .is_some_and(|s| s.chars().any(char::is_control))
        {
            return Err(failed("GitHub 凭据无效"));
        }
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent("Eve-Developer/0.1")
            .build()
            .map_err(|_| failed("GitHub HTTP 客户端不可用"))?;
        Ok(Self {
            config,
            client,
            token,
            root,
            journal,
            next: AtomicU64::new(next),
            serial: Mutex::new(()),
            api: Url::parse("https://api.github.com/").expect("static URL"),
            authorization: None,
            member_runtime: None,
            isolated: false,
            command_policy: None,
            action_reviewer: None,
        })
    }
    pub fn with_authorization(
        mut self,
        authorization: std::sync::Arc<dyn DeveloperAuthorization>,
    ) -> Self {
        self.authorization = Some(authorization);
        self
    }
    /// 模型请求的 run_command 先经命令策略判断；宿主自己发起的构建命令不经过它。
    pub fn with_command_policy(
        mut self,
        policy: std::sync::Arc<dyn eve_developer_api::CommandPolicy>,
    ) -> Self {
        self.command_policy = Some(policy);
        self
    }
    /// 策略要求确认的命令交给审查者按用户原话判断；不配置时拒绝执行，owner_confirmed不能代替审核。
    pub fn with_action_reviewer(mut self, reviewer: std::sync::Arc<dyn ActionReviewer>) -> Self {
        self.action_reviewer = Some(reviewer);
        self
    }
    pub fn with_member_runtime(mut self, runtime: MemberRuntime) -> Self {
        self.member_runtime = Some(runtime);
        self
    }
    fn authorize(&self, context: &ToolExecutionContext) -> Result<(), ToolExecutionError> {
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled("开发操作已取消".into()));
        }
        if context.scope().is_none_or(|s| {
            !self.config.operators.contains(&s.user_id)
                && !self
                    .authorization
                    .as_ref()
                    .is_some_and(|a| a.is_operator(s))
        }) {
            return Err(failed("此开发工具仅供宿主配置的操作者使用"));
        }
        Ok(())
    }
    pub fn authorize_operation(
        &self,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled("开发操作已取消或到期".into()));
        }
        if self.member_operation(operation, context) {
            return Ok(());
        }
        if matches!(operation, DeveloperOperation::Github(_))
            && self.config.github
            && self.config.github_all_users
        {
            if stopped(context) {
                return Err(ToolExecutionError::Cancelled("开发操作已取消".into()));
            }
            if context.scope().is_none_or(|s| s.user_id.trim().is_empty()) {
                return Err(failed("GitHub 工具需要宿主绑定的用户身份"));
            }
            return Ok(());
        }
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled("操作已取消".into()));
        }
        let ordinary = match operation {
            DeveloperOperation::List(a) | DeveloperOperation::Read(a) => !sensitive_path(&a.path),
            DeveloperOperation::Search(a) => !sensitive_path(&a.path),
            DeveloperOperation::Write(a) => !sensitive_path(&a.path),
            DeveloperOperation::Edit(a) => !sensitive_path(&a.path),
            DeveloperOperation::Patch(a) => eve_apply_patch::paths(&a.patch)
                .is_some_and(|paths| paths.iter().all(|p| !sensitive_path(p))),
            DeveloperOperation::Github(a) => {
                a.method == "GET"
                    || (a.method == "POST"
                        && (a.resource == "issues" || a.resource == "pulls" || {
                            let p: Vec<_> = a.resource.split('/').collect();
                            p.len() == 3
                                && p[0] == "issues"
                                && p[1].parse::<u64>().is_ok()
                                && p[2] == "comments"
                        }))
            }
            _ => false,
        };
        if self.config.ordinary_access && ordinary && context.scope().is_some() {
            Ok(())
        } else {
            self.authorize(context)
        }
    }
    fn path(&self, text: &str, new: bool) -> Result<PathBuf, ToolExecutionError> {
        let relative = Path::new(text);
        if relative.is_absolute()
            || text.contains(['\\', ':'])
            || text.chars().any(char::is_control)
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            || relative
                .components()
                .any(|c| matches!(c,Component::Normal(s) if s==".git" || s==".eve-developer"))
        {
            return Err(failed(
                "路径必须位于源码工作区内；Git 管理目录与工具账本不提供文件访问",
            ));
        }
        let path = self.root.join(relative);
        // 文件工具拒绝任何路径组成部分的符号链接；命令本身仍具有宿主权限。
        let mut part = self.root.clone();
        for component in relative.components() {
            if let Component::Normal(name) = component {
                part.push(name);
                match fs::symlink_metadata(&part) {
                    Ok(meta) if meta.file_type().is_symlink() => {
                        return Err(failed("文件工具不跟随符号链接"));
                    }
                    Ok(_) => {}
                    Err(e) if new && e.kind() == std::io::ErrorKind::NotFound && part == path => {}
                    Err(_) => return Err(failed("路径不存在或不可读取；先创建父目录")),
                }
            }
        }
        let parent = if new {
            path.parent().ok_or_else(|| failed("文件路径无效"))?
        } else {
            &path
        };
        if !fs::canonicalize(parent)
            .map_err(|_| failed("路径无法规范化"))?
            .starts_with(&self.root)
        {
            return Err(failed("路径离开源码工作区"));
        }
        Ok(path)
    }
    fn redact(&self, text: &str) -> String {
        match &self.token {
            Some(s) => text.replace(s, "[GITHUB_CREDENTIAL_REDACTED]"),
            None => text.into(),
        }
    }
    fn record(
        &self,
        id: u64,
        kind: &str,
        state: &str,
        result: Option<&Value>,
    ) -> Result<(), ToolExecutionError> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.journal.join("operations.jsonl"))
            .map_err(|_| failed("操作账本不可写；没有开始新操作"))?;
        let value = json!({"format_version":1,"id":id,"kind":kind,"state":state,"success":result.and_then(|v|v.get("success")).and_then(Value::as_bool)});
        writeln!(file, "{value}")
            .and_then(|_| file.sync_all())
            .map_err(|_| failed("操作账本提交失败；操作结果不确定，不能自动重试"))
    }
    /// 独立审核审计只保存请求指纹、等级和结论，不持久化原话、命令参数或模型理由。
    /// 审计写入失败时拒绝执行，不能带着无记录的批准继续。
    fn record_review(
        &self,
        fingerprint: &str,
        result: &Result<ActionReviewVerdict, String>,
    ) -> Result<(), ToolExecutionError> {
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.journal.join("action-reviews.jsonl"))
            .map_err(|_| failed("行动审核审计不可写，命令未执行"))?;
        let value = match result {
            Ok(v) => {
                json!({"format_version":1,"request_sha256":fingerprint,"status":"reviewed","risk":v.risk,"authorization":v.authorization,"allowed":v.permits()})
            }
            Err(_) => {
                json!({"format_version":1,"request_sha256":fingerprint,"status":"failed","allowed":false})
            }
        };
        writeln!(file, "{value}")
            .and_then(|_| file.sync_all())
            .map_err(|_| failed("行动审核审计提交失败，命令未执行"))
    }
    /// 原子替换或新建文件：expected 为空串表示只新建；提交前再次核对文件仍为 expected。
    fn replace_file(
        &self,
        path: &Path,
        expected: &str,
        content: &[u8],
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        let mut tmp = tempfile::NamedTempFile::new_in(path.parent().expect("checked parent"))
            .map_err(|_| failed("临时文件不可创建"))?;
        tmp.write_all(content)
            .and_then(|_| tmp.as_file().sync_all())
            .map_err(|_| failed("文件写入失败，原文件保留"))?;
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled("文件提交已取消".into()));
        }
        // 防止在写临时文件期间被外部编辑覆盖。
        let current = fs::read(path).ok().map(|b| hash(&b));
        if current.as_deref().unwrap_or("") != expected {
            return Err(failed("文件在提交前变化，未覆盖"));
        }
        if expected.is_empty() {
            tmp.persist_noclobber(path)
                .map_err(|_| failed("新文件提交失败，未覆盖已有文件"))?;
        } else {
            tmp.persist(path)
                .map_err(|_| failed("文件原子替换失败，保留待核对状态"))?;
        }
        Ok(())
    }
    /// 多文件补丁：先在内存里算出每个文件的新内容并核对路径，全部成功才开始提交；
    /// 提交时先写（新建与修改），再删除（删除与移动的原文件）。中途失败时按记录恢复已经改动的文件。
    fn patch(
        &self,
        text: &str,
        context: &ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        enum Step {
            Write {
                path: PathBuf,
                rel: String,
                content: Vec<u8>,
                before: Option<Vec<u8>>,
                action: &'static str,
            },
            Delete {
                path: PathBuf,
                rel: String,
                before: Vec<u8>,
            },
        }
        let hunks = eve_apply_patch::parse(text).map_err(failed)?;
        if hunks.len() > 64 {
            return Err(failed("一个补丁至多操作 64 个文件"));
        }
        let read_text = |path: &Path| -> Result<(Vec<u8>, String), ToolExecutionError> {
            let metadata = fs::metadata(path).map_err(|_| failed("文件不可读取"))?;
            if !metadata.is_file() {
                return Err(failed("补丁只操作普通文件，不打开目录、设备或管道"));
            }
            if metadata.len() > MAX_FILE as u64 {
                return Err(failed("文件超过 8 MiB；请用命令处理"));
            }
            let bytes = fs::read(path).map_err(|_| failed("文件不可读取"))?;
            let text =
                String::from_utf8(bytes.clone()).map_err(|_| failed("文件不是 UTF-8 文本"))?;
            Ok((bytes, text))
        };
        let absent = |path: &Path, rel: &str| -> Result<(), ToolExecutionError> {
            match fs::symlink_metadata(path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(failed(format!(
                    "{rel} 已存在；修改已有文件请用 *** Update File"
                ))),
            }
        };
        let mut steps = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for hunk in hunks {
            let mut claim = |rel: &str| {
                if seen.insert(rel.to_string()) {
                    Ok(())
                } else {
                    Err(failed(format!("同一个补丁里 {rel} 出现了不止一次")))
                }
            };
            match hunk {
                eve_apply_patch::Hunk::Add {
                    path: rel,
                    contents,
                } => {
                    claim(&rel)?;
                    let path = self.path(&rel, true)?;
                    absent(&path, &rel)?;
                    steps.push(Step::Write {
                        path,
                        rel,
                        content: contents.into_bytes(),
                        before: None,
                        action: "add",
                    });
                }
                eve_apply_patch::Hunk::Delete { path: rel } => {
                    claim(&rel)?;
                    let path = self.path(&rel, false)?;
                    let (before, _) = read_text(&path)?;
                    steps.push(Step::Delete { path, rel, before });
                }
                eve_apply_patch::Hunk::Update {
                    path: rel,
                    move_to,
                    chunks,
                } => {
                    claim(&rel)?;
                    let path = self.path(&rel, false)?;
                    let (before, original) = read_text(&path)?;
                    let updated = eve_apply_patch::apply_update(&original, &chunks)
                        .map_err(|e| failed(format!("{rel}：{e}；补丁未应用")))?;
                    match move_to {
                        Some(target) => {
                            claim(&target)?;
                            let destination = self.path(&target, true)?;
                            absent(&destination, &target)?;
                            steps.push(Step::Write {
                                path: destination,
                                rel: target,
                                content: updated.into_bytes(),
                                before: None,
                                action: "move",
                            });
                            steps.push(Step::Delete { path, rel, before });
                        }
                        None => steps.push(Step::Write {
                            path,
                            rel,
                            content: updated.into_bytes(),
                            before: Some(before),
                            action: "update",
                        }),
                    }
                }
            }
        }
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled("补丁已取消，未修改".into()));
        }
        steps.sort_by_key(|step| matches!(step, Step::Delete { .. }));
        let mut done: Vec<&Step> = Vec::new();
        let mut results = Vec::new();
        let mut failure = None;
        for step in &steps {
            if stopped(context) {
                failure = Some(ToolExecutionError::Cancelled(
                    "补丁已取消或到期，恢复已提交部分".into(),
                ));
                break;
            }
            let outcome = match step {
                Step::Write {
                    path,
                    rel,
                    content,
                    before,
                    action,
                } => {
                    let expected = before.as_deref().map(hash).unwrap_or_default();
                    self.replace_file(path, &expected, content, context).map(|()| {
                        json!({"path": rel, "action": action, "sha256": hash(content), "bytes": content.len()})
                    })
                }
                Step::Delete { path, rel, before } => {
                    let current = fs::read(path).ok().map(|b| hash(&b));
                    if current.as_deref() != Some(hash(before).as_str()) {
                        Err(failed(format!("{rel} 在提交前变化，未删除")))
                    } else {
                        fs::remove_file(path)
                            .map(|()| json!({"path": rel, "action": "delete"}))
                            .map_err(|_| failed(format!("{rel} 无法删除")))
                    }
                }
            };
            match outcome {
                Ok(result) => {
                    results.push(result);
                    done.push(step);
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        let Some(error) = failure else {
            return Ok(json!({"success": true, "files": results}));
        };
        // 按相反顺序恢复已经提交的改动；恢复本身失败时如实列出，便于人工核对。
        let mut unrestored = Vec::new();
        for step in done.into_iter().rev() {
            let restored = match step {
                Step::Write {
                    path, before: None, ..
                } => fs::remove_file(path).is_ok(),
                Step::Write {
                    path,
                    before: Some(before),
                    ..
                } => fs::write(path, before).is_ok(),
                Step::Delete { path, before, .. } => fs::write(path, before).is_ok(),
            };
            if !restored {
                unrestored.push(match step {
                    Step::Write { rel, .. } | Step::Delete { rel, .. } => rel.clone(),
                });
            }
        }
        if unrestored.is_empty() {
            Err(error)
        } else {
            Err(failed(format!(
                "补丁提交中途失败（{error:?}），以下文件未能恢复，需要人工核对：{}",
                unrestored.join("、")
            )))
        }
    }
    /// 在工作区目录中按字面文本搜索 UTF-8 文件，按目录广度优先、名称排序输出；不跟随符号链接。
    fn search(&self, a: SearchArguments, sensitive: bool) -> Result<Value, ToolExecutionError> {
        let start = self.path(&a.path, false)?;
        if !fs::metadata(&start)
            .map_err(|_| failed("目录不可读取"))?
            .is_dir()
        {
            return Err(failed("搜索只接受工作区内的目录"));
        }
        // 结果路径与 read_file 一致，都相对于工作区根目录。
        let prefix = Path::new(&a.path)
            .components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        let mut queue = VecDeque::from([(start, prefix)]);
        let mut matches = Vec::new();
        let mut used = 0usize;
        let mut files = 0usize;
        let mut truncated = false;
        'walk: while let Some((dir, prefix)) = queue.pop_front() {
            let mut entries = fs::read_dir(&dir)
                .map_err(|_| failed("目录不可读取"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| failed("目录条目不可读取"))?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let kind = entry.file_type().map_err(|_| failed("文件类型不可读取"))?;
                if kind.is_symlink() || SKIPPED_NAMES.contains(&name.as_str()) {
                    continue;
                }
                let relative = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}/{name}")
                };
                if !sensitive && sensitive_path(&relative) {
                    continue;
                }
                if kind.is_dir() {
                    queue.push_back((entry.path(), relative));
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                if files == MAX_SEARCH_FILES {
                    truncated = true;
                    break 'walk;
                }
                files += 1;
                if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE as u64) {
                    continue;
                }
                let Ok(bytes) = fs::read(entry.path()) else {
                    continue;
                };
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                for (index, line) in text.lines().enumerate() {
                    if !line.contains(&a.query) {
                        continue;
                    }
                    // 先脱敏再截断，避免凭据被截断后只露出一部分。
                    let redacted = self.redact(line);
                    let (shown, cut) = shorten(&redacted, MAX_SEARCH_LINE);
                    let item = json!({"path":relative,"line":index+1,"text":shown,"truncated":cut});
                    let size = item.to_string().len();
                    if matches.len() >= a.max_results || used + size > MAX_OUTPUT {
                        truncated = true;
                        break 'walk;
                    }
                    used += size;
                    matches.push(item);
                }
            }
        }
        Ok(
            json!({"query":a.query,"path":a.path,"matches":matches,"files_scanned":files,"truncated":truncated,"untrusted_data":true}),
        )
    }
    async fn execute_inner(
        &self,
        operation: DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        match operation {
            DeveloperOperation::List(a) => {
                let mut entries = fs::read_dir(self.path(&a.path, false)?)
                    .map_err(|_| failed("目录不可读取"))?
                    .map(|r| r.map_err(|_| failed("目录条目不可读取")))
                    .collect::<Result<Vec<_>, _>>()?;
                entries.sort_by_key(|e| e.file_name());
                let total = entries.len();
                let entries=entries.into_iter().take(128).map(|e|{let t=e.file_type().map_err(|_|failed("文件类型不可读取"))?;Ok(json!({"name":e.file_name().to_string_lossy(),"kind":if t.is_symlink(){"symlink"}else if t.is_dir(){"directory"}else{"file"}}))}).collect::<Result<Vec<_>,ToolExecutionError>>()?;
                Ok(json!({"path":a.path,"entries":entries,"total":total,"truncated":total>128}))
            }
            DeveloperOperation::Read(a) => {
                let path = self.path(&a.path, false)?;
                let metadata = fs::metadata(&path).map_err(|_| failed("文件不可读取"))?;
                if !metadata.is_file() {
                    return Err(failed("文件工具只读取普通文件，不打开目录、设备或管道"));
                }
                if metadata.len() > MAX_FILE as u64 {
                    return Err(failed("文件超过 8 MiB；请用命令读取指定范围"));
                }
                let bytes = fs::read(path).map_err(|_| failed("文件不可读取"))?;
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| failed("文件不是 UTF-8 文本"))?;
                let tail = text
                    .get(a.offset..)
                    .ok_or_else(|| failed("offset 必须为有效 UTF-8 字节边界"))?;
                let (content, truncated) = shorten(tail, MAX_OUTPUT);
                Ok(
                    json!({"path":a.path,"sha256":hash(&bytes),"size":bytes.len(),"offset":a.offset,"next_offset":a.offset+content.len(),"content":self.redact(content),"truncated":truncated,"untrusted_data":true}),
                )
            }
            DeveloperOperation::Write(a) => {
                let path = self.path(&a.path, true)?;
                if fs::metadata(&path).is_ok_and(|m| !m.is_file()) {
                    return Err(failed("文件工具只写普通文件，不打开目录、设备或管道"));
                }
                let old = match fs::read(&path) {
                    Ok(b) => Some(b),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => return Err(failed("已有文件不可读取，未覆盖")),
                };
                let expected = a
                    .expected_sha256
                    .as_deref()
                    .ok_or_else(|| failed("缺少文件修订校验"))?;
                if old.as_ref().map(|b| hash(b)).as_deref().unwrap_or("") != expected {
                    return Err(failed("文件已变化；重新读取后再修改，未覆盖"));
                }
                let content = a.content.ok_or_else(|| failed("缺少文件内容"))?;
                self.replace_file(&path, expected, content.as_bytes(), context)?;
                Ok(
                    json!({"success":true,"path":a.path,"sha256":hash(content.as_bytes()),"bytes":content.len()}),
                )
            }
            DeveloperOperation::Edit(a) => {
                let path = self.path(&a.path, false)?;
                let metadata = fs::metadata(&path).map_err(|_| failed("文件不可读取"))?;
                if !metadata.is_file() {
                    return Err(failed("文件工具只修改普通文件，不打开目录、设备或管道"));
                }
                if metadata.len() > MAX_FILE as u64 {
                    return Err(failed("文件超过 8 MiB；请用命令处理"));
                }
                let bytes = fs::read(&path).map_err(|_| failed("已有文件不可读取，未修改"))?;
                if hash(&bytes) != a.expected_sha256 {
                    return Err(failed("文件已变化；重新读取后再修改，未修改"));
                }
                let text =
                    std::str::from_utf8(&bytes).map_err(|_| failed("文件不是 UTF-8 文本"))?;
                match occurrences(text, &a.old_string) {
                    0 => return Err(failed("要替换的文本不存在，未修改")),
                    1 => {}
                    _ => return Err(failed("要替换的文本出现多次；请提供更长的上下文，未修改")),
                }
                let updated = text.replacen(&a.old_string, &a.new_string, 1);
                self.replace_file(&path, &a.expected_sha256, updated.as_bytes(), context)?;
                Ok(
                    json!({"success":true,"path":a.path,"sha256":hash(updated.as_bytes()),"bytes":updated.len()}),
                )
            }
            DeveloperOperation::Patch(a) => self.patch(&a.patch, context),
            DeveloperOperation::Search(a) => {
                self.search(a, self.isolated || self.authorize(context).is_ok())
            }
            DeveloperOperation::Command(a) => {
                if !self.config.commands {
                    return Err(failed("命令执行未启用"));
                }
                if let Some(policy) = &self.command_policy {
                    let evaluation = policy.evaluate(&a.program, &a.args);
                    match evaluation.decision {
                        Some(eve_developer_api::CommandDecision::Forbidden) => {
                            return Err(failed(format!(
                                "命令被策略禁止：{}。不要换一种写法绕过；需要时请主人自己执行",
                                evaluation.reason()
                            )));
                        }
                        Some(eve_developer_api::CommandDecision::Prompt) => {
                            match &self.action_reviewer {
                                Some(reviewer) => {
                                    let mut request =
                                        review_request(&a, evaluation.reason(), context)?;
                                    request.workspace = self.root.to_string_lossy().into_owned();
                                    request.isolated = self.isolated;
                                    let delegated = request.delegated;
                                    let fingerprint = hash(
                                        serde_json::to_string(&request)
                                            .map_err(|_| failed("审查请求无法序列化"))?
                                            .as_bytes(),
                                    );
                                    let result = match request.validate_capacity() {
                                        Ok(()) => reviewer.review(request).await,
                                        Err(error) => Err(error),
                                    };
                                    self.record_review(&fingerprint, &result)?;
                                    let verdict = result.map_err(|e| {
                                        failed(format!("行动审查没有完成（{e}），命令未执行"))
                                    })?;
                                    if !verdict.permits() {
                                        return Err(failed(format!(
                                            "行动审查未放行，命令未执行：风险 {:?}，用户授权 {:?}。{}{}",
                                            verdict.risk,
                                            verdict.authorization,
                                            verdict.rationale,
                                            if delegated {
                                                "。后台执行没有人在场，需要时在结果里说明，请用户在对话里明确要求后再做"
                                            } else {
                                                "。先在对话里说明要执行什么、影响哪些文件或分支，得到用户明确同意后再试"
                                            }
                                        )));
                                    }
                                }
                                None => {
                                    return Err(failed(format!(
                                        "这条命令需要行动审查，但审查器未配置：{}。命令未执行；owner_confirmed 不能代替独立审核",
                                        evaluation.reason()
                                    )));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                self.command(a, context).await
            }
            DeveloperOperation::Github(a) => self.github(a).await,
            DeveloperOperation::PrepareUpgrade => self.upgrade(context).await,
            DeveloperOperation::ApplyUpgrade(a) => self.apply_upgrade(a, context),
            DeveloperOperation::UpgradeStatus => self.upgrade_status(),
        }
    }
    async fn command(
        &self,
        a: CommandArguments,
        context: &ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        self.command_limited(a, context, MAX_OUTPUT).await
    }
    async fn command_limited(
        &self,
        a: CommandArguments,
        context: &ToolExecutionContext,
        output_limit: usize,
    ) -> Result<Value, ToolExecutionError> {
        let cwd = self.path(&a.cwd, false)?;
        if !cwd.is_dir() {
            return Err(failed("命令 cwd 必须是目录"));
        }
        let mut command = if self.isolated {
            self.member_command(&a, &cwd)?
        } else {
            Command::new(&a.program)
        };
        command
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if !self.isolated {
            command.args(&a.args);
        }
        // 保留正常构建/代理/CA；不把 QQ、模型、Web 和 GitHub 密钥复制给命令。
        for name in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SystemRoot",
            "WINDIR",
            "TEMP",
            "TMP",
            "TMPDIR",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "CARGO_TARGET_DIR",
            "CARGO_INCREMENTAL",
            "CARGO_PROFILE_DEV_DEBUG",
            "CARGO_PROFILE_TEST_DEBUG",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "NODE_EXTRA_CA_CERTS",
            "GIT_SSL_CAINFO",
        ] {
            if !self.isolated
                && let Some(value) = std::env::var_os(name)
            {
                command.env(name, value);
            }
        }
        if self.isolated {
            self.member_environment(&mut command);
        }
        #[cfg(unix)]
        command.process_group(0);
        if stopped(context) {
            return Err(ToolExecutionError::Cancelled(
                "命令准入已取消或到期；未启动".into(),
            ));
        }
        let mut child = command
            .spawn()
            .map_err(|_| failed("程序无法启动；检查 program 与构建环境"))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let mut group = ProcessGroup { pid: child.id() };
        let limit = a
            .timeout_ms
            .unwrap_or(self.config.command_timeout_ms)
            .min(self.config.command_timeout_ms);
        let work = async {
            let (status, out, err) = tokio::join!(
                child.wait(),
                capture(stdout, output_limit),
                capture(stderr, output_limit)
            );
            Ok::<_, ToolExecutionError>((
                status.map_err(|_| failed("命令退出状态无法取得"))?,
                out?,
                err?,
            ))
        };
        let result = tokio::select! {
            result=tokio::time::timeout(Duration::from_millis(limit),work)=>result.map_err(|_|failed("命令执行超时，正在终止进程组；操作可能已有副作用"))?,
            _=cancelled(context)=>return Err(ToolExecutionError::Cancelled("命令已取消；操作可能已有副作用".into())),
        };
        let (status, out, err) = result?;
        group.terminate();
        Ok(
            json!({"success":status.success(),"exit_code":status.code(),"stdout":self.redact(&out.0),"stderr":self.redact(&err.0),"stdout_truncated":out.1,"stderr_truncated":err.1}),
        )
    }
    async fn github(&self, a: GithubArguments) -> Result<Value, ToolExecutionError> {
        if !self.config.github {
            return Err(failed("GitHub 未启用"));
        }
        let a = a.normalized().map_err(failed)?;
        let search = a.resource.starts_with("search/");
        let first = a.resource.split('/').next().unwrap_or("");
        if search {
            if a.method != "GET"
                || ![
                    "search/code",
                    "search/issues",
                    "search/commits",
                    "search/repositories",
                ]
                .contains(&a.resource.as_str())
            {
                return Err(failed("不支持此 GitHub 搜索资源"));
            }
        } else if ![
            "",
            "contents",
            "readme",
            "git",
            "branches",
            "commits",
            "compare",
            "issues",
            "pulls",
            "actions",
            "releases",
            "statuses",
            "check-runs",
            "check-suites",
            "tags",
            "languages",
            "contributors",
        ]
        .contains(&first)
        {
            return Err(failed(
                "GitHub resource 不支持此资源；读取源码请使用 contents/文件路径，仓库信息使用空 resource，搜索使用 search/code 等；不提供账户或密钥管理",
            ));
        }
        if a.method != "GET"
            && ((!self.config.github_all_repositories
                && !self
                    .config
                    .github_write_repositories
                    .contains(&a.repository))
                || self.token.is_none())
        {
            return Err(failed("此仓库的 GitHub 写权限未配置或缺少凭据"));
        }
        // 禁止借 Actions secrets 或 variables 读取宿主/工作流密钥。
        if a.resource
            .split('/')
            .any(|p| matches!(p, "secrets" | "variables"))
        {
            return Err(failed("GitHub 密钥资源不对模型开放"));
        }
        let path = if search {
            a.resource.clone()
        } else if a.resource.is_empty() {
            format!("repos/{}", a.repository)
        } else {
            format!("repos/{}/{}", a.repository, a.resource)
        };
        let mut url = self
            .api
            .join(&path)
            .map_err(|_| failed("GitHub 路径无效"))?;
        if !a.query.is_empty() {
            let mut q = url.query_pairs_mut();
            for (k, v) in &a.query {
                q.append_pair(k, v);
            }
        }
        let method = reqwest::Method::from_bytes(a.method.as_bytes())
            .map_err(|_| failed("GitHub 方法无效"))?;
        let mut request = self
            .client
            .request(method, url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = a.body {
            request = request.json(&body);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| failed("GitHub 请求失败；没有自动重试，写结果可能不确定"))?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| failed("GitHub 响应读取失败；写结果可能不确定"))?
        {
            if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
                return Err(failed("GitHub 响应超过 2 MiB；请分页或读取子资源"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| failed("GitHub 资源不是 UTF-8；二进制资源请使用命令下载"))?;
        let redacted = self.redact(text);
        let data = serde_json::from_str::<Value>(&redacted).unwrap_or(Value::String(redacted));
        // contents 的 Base64 可由 run_command 解码；小文件同时提供真实 UTF-8 源码。
        let encoded = data.get("content").and_then(Value::as_str);
        let decoded = if data.get("encoding").and_then(Value::as_str) == Some("base64") {
            encoded
                .and_then(decode64)
                .and_then(|b| String::from_utf8(b).ok())
        } else {
            None
        };
        let (body, truncated) = if let Some(text) = decoded {
            let (s, t) = shorten(&text, MAX_OUTPUT);
            (
                json!({"path":data.get("path"),"sha":data.get("sha"),"size":data.get("size"),"text":self.redact(s)}),
                t,
            )
        } else {
            let text = data.to_string();
            let (s, t) = shorten(&text, MAX_OUTPUT);
            if t {
                (Value::String(s.into()), true)
            } else {
                (data, false)
            }
        };
        Ok(
            json!({"success":(200..300).contains(&status),"status":status,"repository":a.repository,"resource":a.resource,"data":body,"truncated":truncated,"untrusted_data":true}),
        )
    }
    async fn upgrade(&self, context: &ToolExecutionContext) -> Result<Value, ToolExecutionError> {
        if !self.config.self_upgrade || !self.config.commands {
            return Err(failed("自身升级准备未启用"));
        }
        if !self.root.join("crates/eve-app/Cargo.toml").is_file() {
            return Err(failed("源码工作区不是 Eve 源码仓库"));
        }
        let before = self.source_fingerprint(context).await?;
        let mut checks = vec![];
        for args in [
            vec!["fmt", "--all", "--", "--check"],
            vec![
                "clippy",
                "--release",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            vec!["test", "--release", "--workspace", "--locked"],
            vec!["build", "-p", "eve-app", "--bins", "--release", "--locked"],
        ] {
            let args = self
                .config
                .cargo_arguments
                .iter()
                .cloned()
                .chain(args.iter().map(|s| s.to_string()))
                .collect();
            let result = self
                .command(
                    CommandArguments {
                        owner_confirmed: false,
                        program: self.config.cargo_program.clone(),
                        args,
                        cwd: ".".into(),
                        timeout_ms: None,
                    },
                    context,
                )
                .await?;
            let success = result["success"] == true;
            checks.push(result);
            if !success {
                return Ok(
                    json!({"success":false,"prepared":false,"applied":false,"checks":checks}),
                );
            }
        }
        let after = self.source_fingerprint(context).await?;
        if before != after {
            return Err(failed(
                "验证期间源码发生变化；候选未准备，重新核对源码后再验证",
            ));
        }
        // 从实际 Cargo（含宿主包装器和项目配置）取得构建输出目录，
        // 不能用父进程的环境猜测，以免选中另一个检出的旧可执行文件。
        let metadata = self
            .command_limited(
                CommandArguments {
                    owner_confirmed: false,
                    program: self.config.cargo_program.clone(),
                    args: self
                        .config
                        .cargo_arguments
                        .iter()
                        .cloned()
                        .chain(
                            ["metadata", "--format-version", "1", "--no-deps", "--locked"]
                                .into_iter()
                                .map(str::to_string),
                        )
                        .collect(),
                    cwd: ".".into(),
                    timeout_ms: Some(30_000),
                },
                context,
                2 * 1024 * 1024,
            )
            .await?;
        if metadata["success"] != true || metadata["stdout_truncated"] == true {
            return Err(failed("构建输出目录无法完整核对；没有准备候选"));
        }
        let metadata: Value = serde_json::from_str(
            metadata["stdout"]
                .as_str()
                .ok_or_else(|| failed("Cargo 元数据缺失"))?,
        )
        .map_err(|_| failed("Cargo 元数据无效"))?;
        let target = PathBuf::from(
            metadata["target_directory"]
                .as_str()
                .ok_or_else(|| failed("Cargo 输出目录缺失"))?,
        );
        if !target.is_absolute() {
            return Err(failed("Cargo 输出目录必须是绝对路径"));
        }
        let binary = target.join("release").join(if cfg!(windows) {
            "eve-qqbot.exe"
        } else {
            "eve-qqbot"
        });
        let bytes =
            live_upgrade::read_bounded_scoped(&binary, 2 * 1024 * 1024 * 1024, Some(context))
                .map_err(|error| match error {
                    error @ ToolExecutionError::Cancelled(_) => error,
                    _ => failed("构建程序不可读、类型无效或超过2GiB；没有准备候选"),
                })?;
        self.publish_upgrade(before, bytes, checks, context).await
    }
    async fn source_fingerprint(
        &self,
        context: &ToolExecutionContext,
    ) -> Result<Value, ToolExecutionError> {
        let mut facts = BTreeMap::new();
        for (key, args) in [
            ("head", vec!["rev-parse", "HEAD"]),
            ("diff", vec!["diff", "HEAD", "--"]),
            (
                "untracked",
                vec!["ls-files", "--others", "--exclude-standard"],
            ),
        ] {
            let result = self
                .command(
                    CommandArguments {
                        owner_confirmed: false,
                        program: "git".into(),
                        args: args.into_iter().map(str::to_string).collect(),
                        cwd: ".".into(),
                        timeout_ms: Some(30_000),
                    },
                    context,
                )
                .await?;
            if result["success"] != true || result["stdout_truncated"] == true {
                return Err(failed("源码修订无法完整核对"));
            }
            let text = result["stdout"]
                .as_str()
                .ok_or_else(|| failed("源码修订无法读取"))?;
            if key == "untracked" && !text.trim().is_empty() {
                return Err(failed(
                    "先 git add 新文件，再准备自身升级，以便核对全部源码",
                ));
            }
            facts.insert(
                key.to_string(),
                if key == "head" {
                    text.trim().to_string()
                } else {
                    hash(text.as_bytes())
                },
            );
        }
        Ok(json!(facts))
    }
}
impl DeveloperBackend for LocalDeveloper {
    fn preflight(
        &self,
        operation: &DeveloperOperation,
        context: &ToolExecutionContext,
    ) -> Result<(), ToolExecutionError> {
        self.authorize_operation(operation, context)
    }
    fn execute(
        &self,
        operation: DeveloperOperation,
        context: ToolExecutionContext,
    ) -> ToolFuture<'_> {
        Box::pin(async move {
            self.authorize_operation(&operation, &context)?;
            let _serial = tokio::select! {lock=self.serial.lock()=>lock,_=cancelled(&context)=>return Err(ToolExecutionError::Cancelled("等待中的开发操作已取消".into()))};
            self.authorize_operation(&operation, &context)?;
            let kind = match &operation {
                DeveloperOperation::List(_) => "list",
                DeveloperOperation::Read(_) => "read",
                DeveloperOperation::Write(_) => "write",
                DeveloperOperation::Edit(_) => "edit",
                DeveloperOperation::Patch(_) => "patch",
                DeveloperOperation::Search(_) => "search",
                DeveloperOperation::Command(_) => "command",
                DeveloperOperation::Github(_) => "github",
                DeveloperOperation::PrepareUpgrade => "upgrade",
                DeveloperOperation::ApplyUpgrade(_) => "apply_upgrade",
                DeveloperOperation::UpgradeStatus => "upgrade_status",
            };
            let id = self.next.fetch_add(1, Ordering::SeqCst);
            self.record(id, kind, "started", None)?;
            let member = if self.member_operation(&operation, &context) {
                Some(self.member_backend(&context)?)
            } else {
                None
            };
            let backend = member.as_ref().unwrap_or(self);
            let result = tokio::select! {result=backend.execute_inner(operation,&context)=>result,_=cancelled(&context)=>Err(ToolExecutionError::Cancelled("开发操作已取消；不能假定没有副作用".into()))};
            self.record(
                id,
                kind,
                if result.is_ok() { "finished" } else { "failed" },
                result.as_ref().ok(),
            )?;
            result
        })
    }
}
async fn cancelled(context: &ToolExecutionContext) {
    loop {
        if stopped(context) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
async fn capture(
    mut stream: impl AsyncRead + Unpin,
    limit: usize,
) -> Result<(String, bool), ToolExecutionError> {
    let mut output = Vec::new();
    let mut total = 0;
    let mut buf = [0; 8192];
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|_| failed("命令输出读取失败"))?;
        if n == 0 {
            break;
        }
        total += n;
        let keep = n.min(limit - output.len());
        output.extend_from_slice(&buf[..keep]);
    }
    Ok((
        String::from_utf8_lossy(&output).into_owned(),
        total > output.len(),
    ))
}
struct ProcessGroup {
    pid: Option<u32>,
}
impl ProcessGroup {
    fn terminate(&mut self) {
        if let Some(pid) = self.pid.take() {
            #[cfg(unix)]
            {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(not(any(unix, windows)))]
            let _ = pid;
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}
fn decode64(text: &str) -> Option<Vec<u8>> {
    let mut bits = 0u32;
    let mut count = 0;
    let mut out = vec![];
    for b in text.bytes().filter(|b| !b.is_ascii_whitespace()) {
        if b == b'=' {
            break;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | v as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests;
