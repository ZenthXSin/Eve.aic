//! 运行前的 JSON 内容预检：在交给真实服务端之前，用继承来的规则先查一遍数据文件。
//!
//! 规则来自 `rules` 模块（eve-portable 的原样副本），这里只负责：
//! 选出要送检的文件、在受限解释器里运行调用层、解析它输出的一行 JSON、把每条
//! ERROR/WARN 变成草稿器能看懂的中文问题。预检不写工作目录、不启动游戏；
//! 它给出的问题只写进本次尝试的记录，不改变运行器对“是否真的通过”的判断。

use crate::rules::MindustryJsonRules;
use eve_practice_api::{
    ArtifactFile, DraftCheck, MAX_ISSUE_BYTES, MAX_ISSUES, PracticeDraft, PracticeResult,
    PracticeRun,
};
use std::{ffi::OsString, path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command};

/// 解释器命令；默认 `python3`（Windows 上通常是 `python`），可加前置参数。
#[derive(Clone)]
pub struct PythonCommand {
    pub program: OsString,
    pub prefix_args: Vec<OsString>,
}
impl Default for PythonCommand {
    fn default() -> Self {
        Self {
            program: if cfg!(windows) {
                "python".into()
            } else {
                "python3".into()
            },
            prefix_args: Vec::new(),
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(60);
/// 预检问题的开头；游戏遇到警告仍会加载模组，所以实践只按错误拦下草稿。
const ERROR_MARK: &str = "JSON 预检错误";
const WARNING_MARK: &str = "JSON 预检警告";
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// 送入预检的文件：内容文件，或根目录的 mod 清单。脚本、贴图、包文件永远不会被选中。
fn selected(files: &[ArtifactFile]) -> impl Iterator<Item = &ArtifactFile> {
    files.iter().filter(|file| {
        file.path == "mod.hjson" || file.path == "mod.json" || file.path.starts_with("content/")
    })
}

/// 按调用层约定的文件块格式编码；内容只作为数据，路径已由 `validate_artifact` 限制字符集。
fn encode<'a>(files: impl Iterator<Item = &'a ArtifactFile>) -> Vec<u8> {
    let mut body = Vec::new();
    for file in files {
        body.extend_from_slice(format!("==== {} ====\n", file.path).as_bytes());
        body.extend_from_slice(file.content.as_bytes());
        if !file.content.ends_with('\n') {
            body.push(b'\n');
        }
        body.extend_from_slice(b"==== end ====\n");
    }
    body
}

pub struct JsonPrecheck {
    rules: MindustryJsonRules,
    python: PythonCommand,
}
impl Clone for JsonPrecheck {
    fn clone(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            python: self.python.clone(),
        }
    }
}

impl JsonPrecheck {
    pub fn new(rules: MindustryJsonRules, python: PythonCommand) -> Self {
        Self { rules, python }
    }

    /// 预检一份草稿；解释器缺失、超时或输出不合约定时返回 `Err`，由宿主如实报告，
    /// 不能当作“没有发现问题”。
    pub async fn check(
        &self,
        mod_name: &str,
        files: &[ArtifactFile],
    ) -> Result<Vec<String>, String> {
        let blocks = encode(selected(files));
        if blocks.is_empty() {
            return Ok(Vec::new());
        }
        let mut command = Command::new(&self.python.program);
        command
            .args(&self.python.prefix_args)
            .arg(self.rules.driver())
            .args(["--mod-name", mod_name])
            .current_dir(parent(self.rules.driver()))
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in [
            "PATH",
            "SystemRoot",
            "SYSTEMROOT",
            "PYTHONHOME",
            "PYTHONPATH",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        // 解释器不读用户目录、不写字节码缓存：规则包保持只读。
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        // 管道编码固定为 UTF-8；Windows 默认按本地代码页输出，中文结果会让调用层异常退出。
        command.env("PYTHONIOENCODING", "utf-8");
        let mut child = command
            .spawn()
            .map_err(|_| "无法启动 JSON 预检解释器；检查 Python 是否可用".to_string())?;
        let mut stdin = child.stdin.take().ok_or("无法写入 JSON 预检输入")?;
        let input = blocks;
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
            let _ = stdin.shutdown().await;
        });
        // 超时用带截止时间的等待实现；超时后显式杀掉进程，不留下孤儿解释器。
        let waited = tokio::time::timeout(TIMEOUT, child.wait_with_output()).await;
        let _ = writer.await;
        let output = match waited {
            Ok(Ok(output)) => output,
            Ok(Err(_)) => return Err("JSON 预检进程异常结束".into()),
            Err(_) => {
                // child 已随 wait_with_output 的 future 一起被丢弃，kill_on_drop 生效。
                return Err("JSON 预检超时，已终止".into());
            }
        };
        if output.stdout.len() > MAX_OUTPUT_BYTES {
            return Err("JSON 预检输出超出上限".into());
        }
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "JSON 预检没有正常结束；规则包 {}",
                first_line(&stderr).unwrap_or("无错误输出")
            ));
        }
        let text = String::from_utf8(output.stdout).map_err(|_| "JSON 预检输出不是 UTF-8")?;
        parse(&text)
    }
}

impl JsonPrecheck {
    /// 启动时实跑一次最小清单：解释器缺失、规则包无法导入或输出不合约定时返回原因，
    /// 由宿主拒绝启动，而不是等到第一次实践才发现。
    pub async fn probe(&self) -> Result<(), String> {
        let manifest = ArtifactFile {
            path: "mod.hjson".into(),
            content: "name: \"eve-probe\"\nversion: \"1\"\nminGameVersion: 146\n".into(),
        };
        self.check("eve-probe", std::slice::from_ref(&manifest))
            .await
            .map(drop)
    }
}

/// 把 JSON 预检接成实践运行前检查：取 mod 清单的 name 作为命名前缀，送检清单与内容文件。
///
/// 只有错误会作为运行前问题拦下这次运行；警告（例如缺少 name 字段而按文件名推断）游戏照常加载，
/// 不交给草稿器，以免正确的草稿永远跑不到真实运行。
///
/// 预检只是“值不值得实际运行”的提示，是否通过以真实运行为准。所以解释器在运行中失败、
/// 超时或输出不合约定时，这次尝试不加预检问题、照常实际运行，并把原因交给 `on_unavailable`
/// 由宿主如实记录；不能当作“没有发现问题”报告给用户，也不让实践因此失败。
pub struct JsonDraftCheck {
    precheck: JsonPrecheck,
    on_unavailable: Arc<dyn Fn(&str) + Send + Sync>,
}

impl JsonDraftCheck {
    pub fn new(precheck: JsonPrecheck, on_unavailable: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Self {
            precheck,
            on_unavailable,
        }
    }
}

impl DraftCheck for JsonDraftCheck {
    fn check(
        &self,
        _run: &PracticeRun,
        draft: &PracticeDraft,
        _at_ms: u64,
    ) -> PracticeResult<Vec<String>> {
        let mod_name = mod_name(&draft.files);
        // 同步契约：在独立线程的单线程运行时里等待规则跑完（单次 60 秒上限），
        // 避免在调用方的异步运行时里嵌套阻塞。
        let precheck = self.precheck.clone();
        let files = draft.files.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "无法创建 JSON 预检运行时".to_string())
                .and_then(|runtime| runtime.block_on(precheck.check(&mod_name, &files)))
        })
        .join()
        .unwrap_or_else(|_| Err("JSON 预检线程异常结束".into()));
        match result {
            // 运行前检查的任何问题都会拦下这次运行；警告不影响游戏加载，只把错误交给草稿器。
            Ok(issues) => Ok(issues
                .into_iter()
                .filter(|issue| issue.starts_with(ERROR_MARK))
                .collect()),
            Err(reason) => {
                (self.on_unavailable)(&reason);
                Ok(Vec::new())
            }
        }
    }
}

/// 清单里的 name；没有清单或没写 name 时用 `mod`。
fn mod_name(files: &[ArtifactFile]) -> String {
    files
        .iter()
        .filter(|file| file.path == "mod.hjson" || file.path == "mod.json")
        .find_map(|file| {
            file.content.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                let key = key.trim().trim_matches('"');
                let value = value.trim().trim_end_matches(',').trim().trim_matches('"');
                (key == "name" && !value.is_empty()).then(|| value.to_string())
            })
        })
        .unwrap_or_else(|| "mod".into())
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new("."))
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// 解析调用层输出的一行 JSON；形状不对时拒绝，不猜测。
fn parse(text: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text.trim()).map_err(|_| "JSON 预检输出不是合法 JSON")?;
    if let Some(error) = value["error"].as_str() {
        return Err(format!("JSON 预检报告：{}", prefix(error, 200)));
    }
    let files = value["files"].as_array().ok_or("JSON 预检输出缺少 files")?;
    let mut issues = Vec::new();
    for file in files {
        let path = file["path"].as_str().unwrap_or_default();
        for message in file["messages"].as_array().into_iter().flatten() {
            let level = message["level"].as_str().unwrap_or("ERROR");
            let text = message["message"].as_str().unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            let mark = if level == "WARN" {
                WARNING_MARK
            } else {
                ERROR_MARK
            };
            issues.push(format!(
                "{mark}：{}（{}）",
                prefix(text, MAX_ISSUE_BYTES.saturating_sub(path.len() + 32)),
                path
            ));
            if issues.len() >= MAX_ISSUES {
                return Ok(issues);
            }
        }
    }
    Ok(issues)
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
