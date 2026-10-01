//! 与真实入口共享的验收流程；只通过进程、公开状态后端与会话契约核对。
use eve_kernel::backends::FileStateStore;
use eve_llm_api::{ChatMessage, ToolOutput};
use eve_plugin_api::{PluginId, StateStore};
use eve_session_api::{SessionSnapshot, SessionTurnStatus, SESSION_PLUGIN_ID};
use eve_session_plugin::SESSION_STATE_KEY;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: u64 = 1024 * 1024;

pub struct CheckOptions {
    pub directory: PathBuf,
    pub agent_path: PathBuf,
    /// 每次 Provider 等待的上限；每轮最多两次请求，额外工具调用仍会验收失败。
    pub request_timeout: Duration,
}
#[derive(Debug)]
pub struct CheckError {
    pub stage: &'static str,
    pub code: &'static str,
    pub report_write_failed: bool,
}
impl CheckError {
    fn new(stage: &'static str, code: &'static str) -> Self {
        Self { stage, code, report_write_failed: false }
    }
    pub fn report(&self) -> Value {
        json!({"format_version":1,"status":"failed","stage":self.stage,
            "code":self.code,"report_write_failed":self.report_write_failed})
    }
}
impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "验收失败：{} / {}", self.stage, self.code)
    }
}
impl std::error::Error for CheckError {}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        // 错误/期限路径也回收实际进程；不能只丢弃 wait。
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn process(
    mut command: Command,
    input: &str,
    directory: &Path,
    name: &str,
    deadline: Duration,
    stage: &'static str,
) -> Result<String, CheckError> {
    let stdout_path = directory.join(format!("{name}.stdout"));
    let stdout = private_file(&stdout_path).map_err(|_| CheckError::new(stage, "output_file"))?;
    let stderr = private_file(&directory.join(format!("{name}.stderr")))
        .map_err(|_| CheckError::new(stage, "output_file"))?;
    // 输出写独立文件，避免等待进程退出时 stdout/stderr 管道填满死锁。
    let mut child = Running(command.stdin(Stdio::piped())
        .stdout(stdout).stderr(stderr).spawn()
        .map_err(|_| CheckError::new(stage, "spawn"))?);
    let mut stdin = child.0.stdin.take().ok_or_else(|| CheckError::new(stage, "stdin"))?;
    stdin.write_all(input.as_bytes()).map_err(|_| CheckError::new(stage, "stdin"))?;
    drop(stdin); // EOF 完成本进程的已接收轮次；不发送立即退出的 /quit。
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait().map_err(|_| CheckError::new(stage, "wait"))? {
            break status;
        }
        if started.elapsed() >= deadline {
            return Err(CheckError::new(stage, "deadline"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        return Err(CheckError::new(stage, "child_failed"));
    }
    let mut bytes = Vec::new();
    File::open(stdout_path).map_err(|_| CheckError::new(stage, "output_read"))?
        .take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes)
        .map_err(|_| CheckError::new(stage, "output_read"))?;
    if bytes.len() as u64 > OUTPUT_LIMIT {
        return Err(CheckError::new(stage, "output_limit"));
    }
    String::from_utf8(bytes).map_err(|_| CheckError::new(stage, "output_encoding"))
}
fn snapshot(directory: &Path, stage: &'static str) -> Result<SessionSnapshot, CheckError> {
    let state = directory.join("state");
    if !state.join("state.json").is_file() {
        return Err(CheckError::new(stage, "state_missing"));
    }
    let backend = FileStateStore::open(state).map_err(|_| CheckError::new(stage, "state_open"))?;
    let bytes = backend.get(
        &PluginId::new(SESSION_PLUGIN_ID).expect("有效内置 ID"), SESSION_STATE_KEY,
    ).map_err(|_| CheckError::new(stage, "state_read"))?
        .ok_or_else(|| CheckError::new(stage, "session_missing"))?;
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|_| CheckError::new(stage, "session_document"))?;
    if document["format_version"] != 1
        || document["sessions"].as_object().is_none_or(|s| s.len() != 1)
    {
        return Err(CheckError::new(stage, "session_document"));
    }
    let snapshot: SessionSnapshot = serde_json::from_value(document["sessions"]["default"].clone())
        .map_err(|_| CheckError::new(stage, "session_document"))?;
    snapshot.validate().map_err(|_| CheckError::new(stage, "session_invalid"))?;
    if snapshot.key.session_id != "default" || snapshot.key.user_id != "owner" {
        return Err(CheckError::new(stage, "session_owner"));
    }
    Ok(snapshot)
}
fn completed<'a>(snapshot: &'a SessionSnapshot, index: usize, stage: &'static str) -> Result<&'a [ChatMessage], CheckError> {
    match snapshot.turns.get(index).map(|t| &t.status) {
        Some(SessionTurnStatus::Completed { messages }) => Ok(messages),
        _ => Err(CheckError::new(stage, "turn_incomplete")),
    }
}
fn verify(snapshot: &SessionSnapshot, marker: &str, turns: usize, stage: &'static str) -> Result<(), CheckError> {
    if snapshot.turns.len() != turns || snapshot.revision != (turns * 2) as u64 {
        return Err(CheckError::new(stage, "turn_count"));
    }
    for index in 0..turns {
        let messages = completed(snapshot, index, stage)?;
        if !messages.last().and_then(|m| m.text.as_deref()).is_some_and(|t| t.contains(marker)) {
            return Err(CheckError::new(stage, "reply_marker"));
        }
        let calls: Vec<_> = messages.iter().flat_map(|m| &m.tool_calls).collect();
        let results: Vec<_> = messages.iter().flat_map(|m| &m.tool_results).collect();
        if index == 0 {
            if calls.len() != 1 || results.len() != 1
                || calls[0].name != "echo" || calls[0].arguments != json!({"text":marker})
                || results[0].call_id != calls[0].id
                || results[0].output != ToolOutput::Success(json!({"echo":marker}))
            {
                return Err(CheckError::new(stage, "tool_round_trip"));
            }
        } else if !calls.is_empty() || !results.is_empty() {
            return Err(CheckError::new(stage, "unexpected_tool"));
        }
        if index > 0 && snapshot.turns[index].input.contains(marker) {
            return Err(CheckError::new(stage, "marker_reintroduced"));
        }
    }
    Ok(())
}
fn scenario(
    options: &CheckOptions,
    marker: &str,
    mut command: impl FnMut() -> Command,
) -> Result<Value, CheckError> {
    let started = Instant::now();
    let first_input = format!(
        "这是 Eve 核心验收。请只调用一次 echo 工具，参数 text 为 {marker}。收到结果后回复完整口令 {marker}，并记住供后续对话使用。\n请从上一轮完成的对话中回复 echo 返回的完整验收口令。不要调用工具，不要编造口令。\n"
    );
    let make = |mut command: Command| {
        command.arg("--state-dir").arg(options.directory.join("state"))
            .arg("--agent").arg(&options.agent_path)
            .args(["--session", "default", "--user", "owner"]);
        command
    };
    let first = process(make(command()), &first_input, &options.directory, "first",
        options.request_timeout * 4 + Duration::from_secs(10), "first_process")?;
    let before = snapshot(&options.directory, "first_checkpoint")?;
    verify(&before, marker, 2, "first_checkpoint")?;
    if first.matches(marker).count() < 2 {
        return Err(CheckError::new("first_checkpoint", "console_reply"));
    }
    let after_output = process(make(command()),
        "这是新进程的恢复验收。请从已有完成历史中回复之前 echo 返回的完整验收口令。不要调用工具，不要编造口令。\n",
        &options.directory, "restart",
        options.request_timeout * 2 + Duration::from_secs(10), "restart_process")?;
    let after = snapshot(&options.directory, "restart_checkpoint")?;
    verify(&after, marker, 3, "restart_checkpoint")?;
    if after.turns[..2] != before.turns || !after_output.contains(marker) {
        return Err(CheckError::new("restart_checkpoint", "history_changed"));
    }
    Ok(json!({"format_version":1,"status":"passed","completed_turns":3,
        "first_process_turns":2,"restored_turns":2,"first_process_tool_calls":1,
        "restart_tool_calls":0,"revision_before":before.revision,"revision_after":after.revision,
        "reply_marker_verified":true,"history_prefix_unchanged":true,
        "elapsed_ms":started.elapsed().as_millis()}))
}

/// 全新目录是一次验收；任何失败都保留已有字节，不自动重跑或删除。
pub fn run(
    options: &CheckOptions,
    marker: &str,
    command: impl FnMut() -> Command,
) -> Result<Value, CheckError> {
    if marker.is_empty() || marker.len() > 128 || !marker.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || options.request_timeout.is_zero() || options.request_timeout > Duration::from_secs(600)
    {
        return Err(CheckError::new("preflight", "invalid_options"));
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&options.directory).map_err(|e| CheckError::new("preflight",
        if e.kind() == std::io::ErrorKind::AlreadyExists { "directory_exists" } else { "directory_create" }))?;
    let result = scenario(options, marker, command);
    let report = match &result { Ok(report) => report.clone(), Err(error) => error.report() };
    let saved = (|| -> std::io::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&options.directory)?;
        serde_json::to_writer_pretty(file.as_file_mut(), &report).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist_noclobber(options.directory.join("acceptance.json")).map_err(|e| e.error)?;
        Ok(())
    })();
    match (result, saved) {
        (result, Ok(())) => result,
        (Err(mut error), Err(_)) => { error.report_write_failed = true; Err(error) }
        (Ok(_), Err(_)) => Err(CheckError { stage:"report",code:"report_write",report_write_failed:true }),
    }
}
