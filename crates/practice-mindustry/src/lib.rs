//! Mindustry 内容模组的实践运行器：在全新目录中用操作者提供的无头服务端实际加载草稿，
//! 查询加载状态与内容属性，采集版本、警告和日志。只接受纯数据文件；
//! 探测查询由运行器按受限标识构造，不执行草稿或模型提供的代码。
mod layout;

use eve_practice_api::{
    BoxFuture, MAX_ISSUE_BYTES, MAX_ISSUES, MAX_LOG_EXCERPT_BYTES, PracticeDraft, PracticeRunner,
    ProbeResult, RunEvidence, RunExit, RunnerProfile,
};
use ring::digest::{Context, SHA256};
use std::{
    ffi::OsString,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};

pub const RUNNER_ID: &str = "mindustry-content-mod:v1";
const DOMAIN: &str = "Mindustry（工厂建造塔防游戏）的内容模组：只由 mod.hjson 与 content 目录下的 hjson/json 数据文件组成，在操作者提供的无头服务端中实际加载，并查询内容的类型与属性。不能运行脚本、Java 代码，也不包含贴图。";
const LAYOUT: &str = "根目录必须有 mod.hjson，含 name（小写字母、数字、-）、displayName、author、version、minGameVersion（数字）。内容文件只能放在 content/blocks、content/items、content/liquids、content/units 下，扩展名 .hjson 或 .json，文件名（小写字母、数字、-）即内容名，游戏中的完整名称为 <mod name>-<文件名>。type 只能写不带包名的类型名（例如 Wall、Conveyor、Item）；requirements 只能引用游戏中存在的物品。不得包含 scripts 目录、.js、.java、.class、.jar 或贴图，mod.hjson 不得包含 main 或 java 字段。探测的 subject 必须是本模组内容的完整名称；property 可取 exists（\"true\" 或 \"false\"）、class（类型名）或内容字段名，数值按游戏中的实际数值比较。";
const PROPERTIES: [&str; 15] = [
    "exists",
    "class",
    "health",
    "size",
    "category",
    "itemCapacity",
    "liquidCapacity",
    "hardness",
    "cost",
    "flammability",
    "explosiveness",
    "radioactivity",
    "charge",
    "speed",
    "armor",
];
const NULL: &str = "@@eve:null@@";
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// 启动运行环境的命令；默认 `java`，可加前置参数（例如包装脚本）。
#[derive(Clone)]
pub struct RuntimeCommand {
    pub program: OsString,
    pub prefix_args: Vec<OsString>,
}
impl Default for RuntimeCommand {
    fn default() -> Self {
        Self {
            program: "java".into(),
            prefix_args: Vec::new(),
        }
    }
}

pub struct MindustryServerRunner {
    profile: RunnerProfile,
    command: RuntimeCommand,
    server_jar: PathBuf,
}

impl MindustryServerRunner {
    /// 读取并摘要操作者提供的服务端 jar；摘要写入运行环境标识，用于复验。
    pub fn new(command: RuntimeCommand, server_jar: PathBuf) -> Result<Self, String> {
        let mut file =
            std::fs::File::open(&server_jar).map_err(|_| "无法读取 Mindustry 服务端 jar")?;
        let mut digest = Context::new(&SHA256);
        let mut buffer = vec![0; 64 * 1024];
        let mut first = true;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| "无法读取 Mindustry 服务端 jar")?;
            if read == 0 {
                break;
            }
            if first && !buffer[..read].starts_with(b"PK") {
                return Err("Mindustry 服务端必须是 jar 文件".into());
            }
            first = false;
            digest.update(&buffer[..read]);
        }
        if first {
            return Err("Mindustry 服务端 jar 为空".into());
        }
        let profile = RunnerProfile {
            runner_id: RUNNER_ID.into(),
            runtime: format!(
                "Mindustry server jar sha256:{}",
                hex(digest.finish().as_ref())
            ),
            domain: DOMAIN.into(),
            layout: LAYOUT.into(),
            properties: PROPERTIES.iter().map(|value| (*value).into()).collect(),
        };
        Ok(Self {
            profile,
            command,
            server_jar,
        })
    }

    async fn execute(&self, draft: &PracticeDraft, workspace: &Path) -> RunEvidence {
        let started = Instant::now();
        let files: Vec<(&str, &str)> = draft
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.content.as_str()))
            .collect();
        let Ok(layout) = layout::inspect(&files) else {
            return failed(RunExit::StartFailed, "草稿未通过结构检查", started);
        };
        let root = workspace.join("config").join("mods").join(&layout.mod_name);
        for file in &draft.files {
            let path = root.join(&file.path);
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, &file.content));
            if written.is_err() {
                return failed(RunExit::StartFailed, "无法写入工作目录", started);
            }
        }
        let mut command = Command::new(&self.command.program);
        command
            .args(&self.command.prefix_args)
            .args(["-Xmx512m", "-Djava.awt.headless=true", "-jar"])
            .arg(&self.server_jar)
            .current_dir(workspace)
            .env_clear();
        for name in ["PATH", "SystemRoot", "SYSTEMROOT", "JAVA_HOME"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        // 运行环境的家目录与临时目录都限定在工作目录内，不写入用户目录。
        for name in ["HOME", "USERPROFILE", "TEMP", "TMP"] {
            command.env(name, workspace);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let Ok(mut child) = command.spawn() else {
            return failed(RunExit::StartFailed, "无法启动运行环境", started);
        };
        let (sender, mut lines) = mpsc::unbounded_channel();
        for reader in [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn tokio::io::AsyncRead + Unpin + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let sender = sender.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(reader).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if sender.send(clean(&line)).is_err() {
                        return;
                    }
                }
            });
        }
        drop(sender);
        let mut capture = Capture::default();
        let workspace_text = workspace.to_string_lossy().to_string();
        let exit = 'run: {
            if !capture
                .until(&mut lines, STARTUP_TIMEOUT, |line| {
                    line.level == 'I' && line.text.contains("Server loaded")
                })
                .await
            {
                break 'run capture.exit;
            }
            let mut script =
                String::from("version\njs \"@@eve:version@@\"\nmods\njs \"@@eve:mods@@\"\n");
            script.push_str(&format!(
                "js (function(m){{return String(m!=null&&m.enabled());}})(Vars.mods.getMod(\"{}\"))\njs \"@@eve:loaded@@\"\n",
                layout.mod_name
            ));
            for (index, probe) in draft.probes.iter().enumerate() {
                script.push_str(&format!(
                    "js {}\njs \"@@eve:probe:{index}@@\"\n",
                    query(&probe.subject, &probe.property)
                ));
            }
            script.push_str("js \"@@eve:end@@\"\n");
            let Some(stdin) = child.stdin.as_mut() else {
                break 'run RunExit::Crashed;
            };
            if stdin.write_all(script.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                break 'run RunExit::Crashed;
            }
            if !capture
                .until(&mut lines, QUERY_TIMEOUT, |line| line.text == "@@eve:end@@")
                .await
            {
                break 'run capture.exit;
            }
            RunExit::Completed
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"exit\n").await;
            let _ = stdin.flush().await;
        }
        if tokio::time::timeout(EXIT_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        capture.evidence(exit, draft, &layout, &workspace_text, started)
    }
}

impl PracticeRunner for MindustryServerRunner {
    fn profile(&self) -> &RunnerProfile {
        &self.profile
    }

    fn check(&self, draft: &PracticeDraft) -> Vec<String> {
        let files: Vec<(&str, &str)> = draft
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.content.as_str()))
            .collect();
        let layout = match layout::inspect(&files) {
            Ok(layout) => layout,
            Err(issues) => return issues,
        };
        draft
            .probes
            .iter()
            .filter(|probe| !layout.content.contains(&probe.subject))
            .map(|probe| {
                format!(
                    "探测对象 {} 不是本模组的内容；可用：{}",
                    probe.subject,
                    layout.content.join("、")
                )
            })
            .collect()
    }

    fn run<'a>(
        &'a self,
        draft: &'a PracticeDraft,
        workspace: &'a Path,
    ) -> BoxFuture<'a, RunEvidence> {
        Box::pin(self.execute(draft, workspace))
    }
}

/// 由受限标识构造查询；subject 与 property 已经过字符集与白名单校验。
fn query(subject: &str, property: &str) -> String {
    let lookup = format!(
        "(function(n){{return Vars.content.block(n)||Vars.content.item(n)||Vars.content.liquid(n)||Vars.content.unit(n);}})(\"{subject}\")"
    );
    match property {
        "exists" => format!("String({lookup}!=null)"),
        "class" => format!(
            "(function(c){{return c==null?\"{NULL}\":String(c.getClass().getSimpleName());}})({lookup})"
        ),
        field => format!(
            "(function(c){{return c==null||c.{field}===undefined||c.{field}===null?\"{NULL}\":String(c.{field});}})({lookup})"
        ),
    }
}

struct Line {
    level: char,
    text: String,
}

/// 去掉终端颜色并拆出日志级别：`[时间] [I] 文本`。其他行级别记为空格。
fn clean(raw: &str) -> String {
    let mut output = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(value) = chars.next() {
        if value == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for value in chars.by_ref() {
                    if value.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if !value.is_control() || value == '\t' {
            output.push(value);
        }
    }
    output
}

fn parse(line: &str) -> Line {
    if let Some(rest) = line.strip_prefix('[')
        && let Some((_, rest)) = rest.split_once("] [")
        && let Some((level, text)) = rest.split_once("] ")
        && level.len() == 1
    {
        return Line {
            level: level.chars().next().unwrap_or(' '),
            text: text.trim_end().to_string(),
        };
    }
    Line {
        level: ' ',
        text: line.trim_end().to_string(),
    }
}

struct Capture {
    lines: Vec<Line>,
    raw: String,
    exit: RunExit,
}
impl Default for Capture {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            raw: String::new(),
            exit: RunExit::Crashed,
        }
    }
}

impl Capture {
    /// 读取输出直到条件满足；超时记 Timeout，输出结束记 Crashed。
    async fn until(
        &mut self,
        lines: &mut mpsc::UnboundedReceiver<String>,
        limit: Duration,
        done: impl Fn(&Line) -> bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            match tokio::time::timeout_at(deadline, lines.recv()).await {
                Err(_) => {
                    self.exit = RunExit::Timeout;
                    return false;
                }
                Ok(None) => {
                    self.exit = RunExit::Crashed;
                    return false;
                }
                Ok(Some(raw)) => {
                    if self.raw.len() + raw.len() < MAX_CAPTURE_BYTES {
                        self.raw.push_str(&raw);
                        self.raw.push('\n');
                    }
                    let line = parse(&raw);
                    let matched = done(&line);
                    self.lines.push(line);
                    if matched {
                        return true;
                    }
                }
            }
        }
    }

    fn evidence(
        self,
        exit: RunExit,
        draft: &PracticeDraft,
        layout: &layout::Layout,
        workspace: &str,
        started: Instant,
    ) -> RunEvidence {
        let redact = |text: &str| text.replace(workspace, "<workspace>");
        let loaded_at = self
            .lines
            .iter()
            .position(|line| line.level == 'I' && line.text.contains("Server loaded"));
        // 启动阶段的警告和错误都计入；正常的服务端启动不产生这类日志。
        let startup = &self.lines[..loaded_at.unwrap_or(self.lines.len())];
        let mut warnings: Vec<String> = startup
            .iter()
            .filter(|line| matches!(line.level, 'W' | 'E'))
            .map(|line| {
                prefix(&redact(&line.text), MAX_ISSUE_BYTES)
                    .trim()
                    .to_string()
            })
            .filter(|text| !text.is_empty())
            .collect();
        warnings.truncate(MAX_ISSUES);
        let segment = |tag: &str| -> Option<Vec<&Line>> {
            let marker = format!("@@eve:{tag}@@");
            let end = self.lines.iter().position(|line| line.text == marker)?;
            let start = self.lines[..end]
                .iter()
                .rposition(|line| {
                    line.text.starts_with("@@eve:") || line.text.contains("Server loaded")
                })
                .map_or(0, |index| index + 1);
            Some(self.lines[start..end].iter().collect())
        };
        let value = |tag: &str| -> Option<String> {
            let lines = segment(tag)?;
            match lines.as_slice() {
                [line] if line.level == 'I' && line.text != NULL => Some(line.text.clone()),
                _ => None,
            }
        };
        let runtime_version = segment("version")
            .and_then(|lines| {
                lines
                    .iter()
                    .find_map(|line| line.text.strip_prefix("Version: ").map(str::to_string))
            })
            .map(|version| prefix(&version, MAX_ISSUE_BYTES).to_string())
            .unwrap_or_default();
        let loaded = value("loaded").as_deref() == Some("true");
        let probes: Vec<ProbeResult> = if exit == RunExit::Completed {
            draft
                .probes
                .iter()
                .enumerate()
                .map(|(index, probe)| {
                    let actual = value(&format!("probe:{index}"))
                        .map(|actual| prefix(&actual, 256).to_string());
                    let passed = actual
                        .as_deref()
                        .is_some_and(|actual| same(actual, &probe.expected));
                    ProbeResult {
                        probe: probe.clone(),
                        actual,
                        passed,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut excerpt = String::new();
        let mut push = |line: String| {
            if excerpt.len() + line.len() < MAX_LOG_EXCERPT_BYTES {
                excerpt.push_str(&line);
                excerpt.push('\n');
            }
        };
        if !runtime_version.is_empty() {
            push(format!("Version: {runtime_version}"));
        }
        for line in startup
            .iter()
            .filter(|line| matches!(line.level, 'W' | 'E') || line.text.contains("mods loaded"))
        {
            push(format!("[{}] {}", line.level, redact(&line.text)));
        }
        if let Some(lines) = segment("mods") {
            for line in lines {
                push(format!("[{}] {}", line.level, redact(&line.text)));
            }
        }
        push(format!("mod {} loaded={loaded}", layout.mod_name));
        for result in &probes {
            push(format!(
                "probe {}.{} expected={} actual={} passed={}",
                result.probe.subject,
                result.probe.property,
                result.probe.expected,
                result.actual.as_deref().unwrap_or("<none>"),
                result.passed
            ));
        }
        push(format!("exit={exit:?}"));
        let raw = redact(&self.raw);
        let mut digest = Context::new(&SHA256);
        digest.update(raw.as_bytes());
        RunEvidence {
            runtime_version: runtime_version
                .chars()
                .filter(|value| !value.is_control())
                .collect(),
            exit,
            loaded,
            warnings,
            probes,
            log_excerpt: excerpt,
            log_sha256: hex(digest.finish().as_ref()),
            log_bytes: raw.len() as u64,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }
    }
}

/// 字符串相等；都是数值时按数值比较；布尔值忽略大小写。
fn same(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    if let (Ok(actual), Ok(expected)) = (actual.parse::<f64>(), expected.parse::<f64>()) {
        return (actual - expected).abs() <= 1e-6 * actual.abs().max(expected.abs()).max(1.0);
    }
    matches!(actual, "true" | "false") && actual.eq_ignore_ascii_case(expected)
}

fn failed(exit: RunExit, reason: &str, started: Instant) -> RunEvidence {
    let mut digest = Context::new(&SHA256);
    digest.update(reason.as_bytes());
    RunEvidence {
        runtime_version: String::new(),
        exit,
        loaded: false,
        warnings: vec![],
        probes: vec![],
        log_excerpt: reason.into(),
        log_sha256: hex(digest.finish().as_ref()),
        log_bytes: reason.len() as u64,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_built_only_from_validated_identifiers_and_values_compare_numerically() {
        assert!(query("eve-sample-wall", "health").contains("c.health"));
        assert!(query("eve-sample-wall", "exists").starts_with("String("));
        assert!(same("520", "520.0") && same("0.25", "0.250") && same("true", "TRUE"));
        assert!(!same("520", "521") && !same("defense", "Defense") && !same("Wall", "wall"));
        assert_eq!(
            clean("\u{1b}[1m\u{1b}[94m[I]\u{1b}[0m 1 mods"),
            "[I] 1 mods"
        );
        let line = parse("[10-08-2026 14:53:05] [W] [config/mods/x.hjson] No type");
        assert_eq!(
            (line.level, line.text.as_str()),
            ('W', "[config/mods/x.hjson] No type")
        );
    }
}
