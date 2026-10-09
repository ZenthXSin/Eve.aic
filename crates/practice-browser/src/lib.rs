//! 静态网页的实践运行器：在全新目录中用操作者提供的无头 Chromium 实际打开草稿，
//! 查询元素的计算样式与文字，采集浏览器版本、样式表加载情况和日志。只接受 HTML 与 CSS；
//! 草稿不能含脚本或外部资源，探测脚本由运行器按受限标识构造，不执行草稿或模型提供的代码。
use eve_practice_api::{
    BoxFuture, MAX_ISSUE_BYTES, MAX_ISSUES, MAX_LOG_EXCERPT_BYTES, PracticeDraft, PracticeRunner,
    ProbeResult, RunEvidence, RunExit, RunnerProfile,
};
use ring::digest::{Context, SHA256};
use serde::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, process::Command};

pub const RUNNER_ID: &str = "static-web-page:v1";
const DOMAIN: &str = "静态网页（HTML 与 CSS）：只由 index.html 与 .css 样式文件组成，在操作者提供的无头 Chromium 中实际打开，并查询元素的计算样式与文字。不能包含脚本、图片或任何外部资源。";
const LAYOUT: &str = "根目录必须有 index.html，其他文件只能是 .css 样式表，经 <link rel=\"stylesheet\" href=\"相对路径\"> 引用，也可以写在 <style> 中。不得包含 <script>、on 开头的事件属性、javascript:、http: 或 https: 地址、url()、@import，以及 iframe、object、embed、form、base、img、svg、video、audio、source 等元素或 src、srcset、http-equiv 属性。探测的 subject 是 index.html 中元素的 id（小写字母、数字、-）；property 可取 exists（\"true\" 或 \"false\"）、text（元素文字，连续空白合并为一个空格并去掉首尾空白），或计算样式名（如 color、fontSize），按浏览器计算后的值比较，忽略空白差异，例如颜色写作 rgb(200, 30, 30)、尺寸写作 40px。";
const PROPERTIES: [&str; 24] = [
    "exists",
    "text",
    "color",
    "backgroundColor",
    "fontSize",
    "fontWeight",
    "fontStyle",
    "fontFamily",
    "textAlign",
    "textTransform",
    "textDecorationLine",
    "lineHeight",
    "letterSpacing",
    "display",
    "visibility",
    "opacity",
    "width",
    "height",
    "marginTop",
    "paddingTop",
    "borderTopWidth",
    "borderTopStyle",
    "borderTopColor",
    "borderTopLeftRadius",
];
/// 草稿中不允许出现的写法（不区分大小写）：脚本、外部资源与导航。
const FORBIDDEN: [&str; 21] = [
    "<script",
    "javascript:",
    "http:",
    "https:",
    "url(",
    "@import",
    "<iframe",
    "<object",
    "<embed",
    "<form",
    "<base",
    "<img",
    "<svg",
    "<video",
    "<audio",
    "<source",
    "<frame",
    "<meta http-equiv",
    "http-equiv",
    "src=",
    "srcset",
];
const RUN_PAGE: &str = "eve-run.html";
const RESULTS_OPEN: &str = "<pre id=\"eve-probe-results\">";
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_CAPTURE_BYTES: usize = 512 * 1024;

pub struct BrowserRunner {
    profile: RunnerProfile,
    program: PathBuf,
}

impl BrowserRunner {
    /// 读取并摘要操作者提供的浏览器程序；摘要写入运行环境标识，用于复验。
    pub fn new(program: PathBuf) -> Result<Self, String> {
        let mut file = std::fs::File::open(&program).map_err(|_| "无法读取浏览器程序")?;
        if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Err("浏览器程序必须是文件".into());
        }
        let mut digest = Context::new(&SHA256);
        let mut buffer = vec![0; 256 * 1024];
        let mut empty = true;
        loop {
            let read = file.read(&mut buffer).map_err(|_| "无法读取浏览器程序")?;
            if read == 0 {
                break;
            }
            empty = false;
            digest.update(&buffer[..read]);
        }
        if empty {
            return Err("浏览器程序为空".into());
        }
        let profile = RunnerProfile {
            runner_id: RUNNER_ID.into(),
            runtime: format!(
                "browser executable sha256:{}",
                hex(digest.finish().as_ref())
            ),
            domain: DOMAIN.into(),
            layout: LAYOUT.into(),
            properties: PROPERTIES.iter().map(|value| (*value).into()).collect(),
        };
        Ok(Self { profile, program })
    }

    async fn execute(&self, draft: &PracticeDraft, workspace: &Path) -> RunEvidence {
        let started = Instant::now();
        if !inspect(draft).is_empty() {
            return failed(RunExit::StartFailed, "草稿未通过结构检查", started);
        }
        let site = workspace.join("site");
        for file in &draft.files {
            let path = site.join(&file.path);
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, &file.content));
            if written.is_err() {
                return failed(RunExit::StartFailed, "无法写入工作目录", started);
            }
        }
        let index = draft
            .files
            .iter()
            .find(|file| file.path == "index.html")
            .map(|file| file.content.as_str())
            .unwrap_or_default();
        let page = site.join(RUN_PAGE);
        if std::fs::write(&page, with_probe_script(index, draft)).is_err() {
            return failed(RunExit::StartFailed, "无法写入工作目录", started);
        }
        let Some(url) = file_url(&page) else {
            return failed(RunExit::StartFailed, "工作目录路径无法转换为地址", started);
        };
        let mut command = Command::new(&self.program);
        command
            .args([
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                // 草稿不能引用外部资源；再把所有域名解析指向不存在，确保不访问网络。
                "--host-resolver-rules=MAP * ~NOTFOUND",
            ])
            .arg(format!(
                "--user-data-dir={}",
                workspace.join("profile").display()
            ))
            .arg("--dump-dom")
            .arg(&url)
            .current_dir(workspace)
            .env_clear();
        for name in ["PATH", "SystemRoot", "SYSTEMROOT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        // 浏览器的家目录与配置目录都限定在工作目录内，不写入用户目录。TMPDIR 保持系统默认：
        // 浏览器在其中创建单实例套接字，工作目录路径较深时会超出套接字路径长度上限而无法启动。
        for name in ["HOME", "USERPROFILE", "TEMP", "TMP"] {
            command.env(name, workspace);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let Ok(mut child) = command.spawn() else {
            return failed(RunExit::StartFailed, "无法启动运行环境", started);
        };
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            return failed(RunExit::StartFailed, "无法读取运行环境输出", started);
        };
        let read = async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let limit = MAX_CAPTURE_BYTES as u64;
            let mut limited_out = (&mut stdout).take(limit);
            let mut limited_err = (&mut stderr).take(limit);
            let _ = tokio::join!(
                limited_out.read_to_end(&mut out),
                limited_err.read_to_end(&mut err)
            );
            let status = child.wait().await;
            (out, err, status)
        };
        let (exit, out, err) = match tokio::time::timeout(RUN_TIMEOUT, read).await {
            Ok((out, err, _)) => (RunExit::Completed, out, err),
            Err(_) => (RunExit::Timeout, Vec::new(), Vec::new()),
        };
        let workspace_text = workspace.to_string_lossy().to_string();
        evidence(exit, draft, &out, &err, &workspace_text, started)
    }
}

impl PracticeRunner for BrowserRunner {
    fn profile(&self) -> &RunnerProfile {
        &self.profile
    }

    fn check(&self, draft: &PracticeDraft) -> Vec<String> {
        inspect(draft)
    }

    fn run<'a>(
        &'a self,
        draft: &'a PracticeDraft,
        workspace: &'a Path,
    ) -> BoxFuture<'a, RunEvidence> {
        Box::pin(self.execute(draft, workspace))
    }
}

/// 结构检查：只有 index.html 与 .css；不含脚本、外部资源与禁止的元素；样式表引用指向草稿中的文件；
/// 探测对象是 index.html 中的元素 id。
fn inspect(draft: &PracticeDraft) -> Vec<String> {
    let mut issues = Vec::new();
    let Some(index) = draft.files.iter().find(|file| file.path == "index.html") else {
        return vec!["根目录必须有 index.html".into()];
    };
    let styles: Vec<&str> = draft
        .files
        .iter()
        .filter(|file| file.path != "index.html")
        .map(|file| file.path.as_str())
        .collect();
    for path in &styles {
        if !path.ends_with(".css") {
            issues.push(format!(
                "{path} 不是 .css 样式表；只允许 index.html 与样式表"
            ));
        }
    }
    for file in &draft.files {
        let lower = file.content.to_ascii_lowercase();
        for token in FORBIDDEN {
            if lower.contains(token) {
                issues.push(format!("{} 含有不允许的 {token}", file.path));
            }
        }
        if has_event_attribute(&lower) {
            issues.push(format!("{} 含有 on 开头的事件属性", file.path));
        }
    }
    let lower = index.content.to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(start) = rest.find("<link") {
        let tag = &rest[start..];
        let end = tag.find('>').unwrap_or(tag.len());
        let tag = &tag[..end];
        let stylesheet = attribute(tag, "rel").is_some_and(|rel| rel.trim() == "stylesheet");
        let href = attribute(tag, "href");
        match href {
            Some(href)
                if stylesheet && styles.iter().any(|path| path.eq_ignore_ascii_case(href)) => {}
            _ => {
                issues.push("<link> 只能是 rel=\"stylesheet\"，href 指向草稿中的 .css 文件".into())
            }
        }
        rest = &rest[start + end..];
    }
    for probe in &draft.probes {
        let quoted = [
            format!("id=\"{}\"", probe.subject),
            format!("id='{}'", probe.subject),
        ];
        if !quoted.iter().any(|needle| lower.contains(needle.as_str())) {
            issues.push(format!(
                "探测对象 {} 不是 index.html 中元素的 id",
                probe.subject
            ));
        }
    }
    issues.truncate(MAX_ISSUES);
    issues
}

/// 是否有 `on…=` 形式的事件属性：前面是空白、引号或 `/`，后面是字母，再到 `=`。
fn has_event_attribute(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut index = 0;
    while let Some(found) = lower[index..].find("on") {
        let at = index + found;
        let before = at.checked_sub(1).map(|position| bytes[position]);
        let boundary = before
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'/'));
        let mut end = at + 2;
        while end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            end += 1;
        }
        let named = end > at + 2;
        let mut next = end;
        while next < bytes.len() && bytes[next].is_ascii_whitespace() {
            next += 1;
        }
        if boundary && named && bytes.get(next) == Some(&b'=') {
            return true;
        }
        index = at + 2;
    }
    false
}

/// 取标签中某个属性的值（双引号、单引号或不带引号）。
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = tag;
    while let Some(found) = rest.find(name) {
        let before = rest[..found].chars().last();
        let after = rest[found + name.len()..].trim_start();
        rest = &rest[found + name.len()..];
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let Some(value) = after.strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        return Some(match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let inner = &value[1..];
                &inner[..inner.find(quote).unwrap_or(inner.len())]
            }
            _ => {
                let end = value
                    .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                    .unwrap_or(value.len());
                &value[..end]
            }
        });
    }
    None
}

/// 在页面末尾加入运行器自己的探测脚本；对象与属性都已经过字符集与白名单校验。
fn with_probe_script(index: &str, draft: &PracticeDraft) -> String {
    let probes: Vec<String> = draft
        .probes
        .iter()
        .map(|probe| format!("[\"{}\",\"{}\"]", probe.subject, probe.property))
        .collect();
    let script = format!(
        "<script>(function(){{var r={{ua:navigator.userAgent,sheets:[],probes:[]}};\
var l=document.querySelectorAll('link[rel~=\"stylesheet\"]');\
for(var i=0;i<l.length;i++){{r.sheets.push({{href:l[i].getAttribute('href'),loaded:!!l[i].sheet}});}}\
var q=[{}];\
for(var j=0;j<q.length;j++){{var e=document.getElementById(q[j][0]),p=q[j][1],v=null;\
if(p==='exists'){{v=String(!!e);}}else if(e){{v=p==='text'?e.textContent.replace(/\\s+/g,' ').trim():String(getComputedStyle(e)[p]);}}\
r.probes.push(v);}}\
var o=document.createElement('pre');o.id='eve-probe-results';o.textContent=JSON.stringify(r);\
document.documentElement.appendChild(o);}})();</script>",
        probes.join(",")
    );
    let lower = index.to_ascii_lowercase();
    match lower.rfind("</body>") {
        Some(at) => format!("{}{script}{}", &index[..at], &index[at..]),
        None => format!("{index}{script}"),
    }
}

/// 把本地路径转换为 file 地址；只保留不需编码的字符，其余按 UTF-8 字节百分号编码。
fn file_url(path: &Path) -> Option<String> {
    let absolute = std::path::absolute(path).ok()?;
    let text = absolute.to_str()?.replace('\\', "/");
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    })
}

#[derive(Deserialize)]
struct Sheet {
    href: Option<String>,
    loaded: bool,
}

#[derive(Deserialize)]
struct Results {
    ua: String,
    sheets: Vec<Sheet>,
    probes: Vec<Option<String>>,
}

/// 从导出的页面中取出探测结果；--dump-dom 输出的文字只转义 `&`、`<`、`>` 与不换行空格。
fn results(dom: &str) -> Option<Results> {
    let start = dom.rfind(RESULTS_OPEN)? + RESULTS_OPEN.len();
    let end = dom[start..].find("</pre>")? + start;
    let text = dom[start..end]
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", "\u{a0}")
        .replace("&amp;", "&");
    serde_json::from_str(&text).ok()
}

/// 浏览器版本取自运行中的 userAgent，例如 `HeadlessChrome/141.0.7390.37`。
fn version(ua: &str) -> String {
    ua.split_whitespace()
        .find_map(|part| {
            part.strip_prefix("HeadlessChrome/")
                .or_else(|| part.strip_prefix("Chrome/"))
                .map(|version| format!("Chromium {version} (headless)"))
        })
        .unwrap_or_default()
}

/// 计算后的值与期望值比较：忽略空白差异，其余逐字。
fn same(actual: &str, expected: &str) -> bool {
    let compact = |text: &str| -> String { text.chars().filter(|c| !c.is_whitespace()).collect() };
    compact(actual) == compact(expected)
}

fn evidence(
    exit: RunExit,
    draft: &PracticeDraft,
    out: &[u8],
    err: &[u8],
    workspace: &str,
    started: Instant,
) -> RunEvidence {
    let redact = |text: &str| text.replace(workspace, "<workspace>");
    let dom = String::from_utf8_lossy(out);
    let parsed = if exit == RunExit::Completed {
        results(&dom).filter(|parsed| parsed.probes.len() == draft.probes.len())
    } else {
        None
    };
    let exit = match (&exit, &parsed) {
        (RunExit::Completed, None) => RunExit::Crashed,
        _ => exit,
    };
    let runtime_version = parsed
        .as_ref()
        .map(|parsed| prefix(&version(&parsed.ua), MAX_ISSUE_BYTES).to_string())
        .unwrap_or_default();
    let mut warnings: Vec<String> = parsed
        .iter()
        .flat_map(|parsed| &parsed.sheets)
        .filter(|sheet| !sheet.loaded)
        .map(|sheet| {
            prefix(
                &format!(
                    "样式表未加载：{}",
                    sheet.href.as_deref().unwrap_or("<无 href>")
                ),
                MAX_ISSUE_BYTES,
            )
            .to_string()
        })
        .collect();
    warnings.truncate(MAX_ISSUES);
    let loaded = parsed.is_some() && warnings.is_empty();
    let probes: Vec<ProbeResult> = match &parsed {
        Some(parsed) => draft
            .probes
            .iter()
            .zip(&parsed.probes)
            .map(|(probe, actual)| {
                let actual = actual
                    .as_deref()
                    .map(|actual| prefix(actual, 256).to_string());
                let passed = actual
                    .as_deref()
                    .is_some_and(|actual| same(actual, &probe.expected));
                ProbeResult {
                    probe: probe.clone(),
                    actual,
                    passed,
                }
            })
            .collect(),
        None => Vec::new(),
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
    for sheet in parsed.iter().flat_map(|parsed| &parsed.sheets) {
        push(format!(
            "stylesheet {} loaded={}",
            redact(sheet.href.as_deref().unwrap_or("<无 href>")),
            sheet.loaded
        ));
    }
    push(format!("page loaded={loaded}"));
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
    // 没有取得探测结果时，附上浏览器最后几行输出作为失败依据。
    if parsed.is_none() {
        let stderr = String::from_utf8_lossy(err);
        let lines: Vec<&str> = stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        for line in &lines[lines.len().saturating_sub(12)..] {
            push(prefix(&redact(line), 300).to_string());
        }
    }
    push(format!("exit={exit:?}"));
    let raw = redact(&format!("{dom}\n{}", String::from_utf8_lossy(err)));
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
    use eve_practice_api::{ArtifactFile, Probe};

    fn draft(index: &str, css: &str) -> PracticeDraft {
        PracticeDraft {
            applicable: true,
            files: vec![
                ArtifactFile {
                    path: "index.html".into(),
                    content: index.into(),
                },
                ArtifactFile {
                    path: "style.css".into(),
                    content: css.into(),
                },
            ],
            probes: vec![Probe {
                subject: "title".into(),
                property: "fontSize".into(),
                expected: "40px".into(),
            }],
            rationale: "页面".into(),
            notes_used: vec![],
        }
    }

    const PAGE: &str = "<!doctype html><html><head><link rel=\"stylesheet\" href=\"style.css\"></head><body><h1 id=\"title\">你好</h1></body></html>";

    #[test]
    fn only_static_html_and_css_without_scripts_or_external_resources_pass_the_check() {
        assert_eq!(
            inspect(&draft(PAGE, "h1 { font-size: 40px; }")),
            Vec::<String>::new()
        );
        for (index, css) in [
            (
                PAGE.replace("</body>", "<script>alert(1)</script></body>"),
                "",
            ),
            (PAGE.replace("<h1 ", "<h1 onclick=\"x()\" "), ""),
            (PAGE.replace("<h1 ", "<h1 ONMouseOver = 'x' "), ""),
            (PAGE.replace("style.css", "other.css"), ""),
            (PAGE.replace("stylesheet", "preload"), ""),
            (PAGE.replace("</body>", "<img src=\"a.png\"></body>"), ""),
            (
                PAGE.replace("</body>", "<a href=\"https://example.com\">x</a></body>"),
                "",
            ),
            (PAGE.replace("id=\"title\"", "id=\"heading\""), ""),
            (PAGE.to_string(), "body { background: url(a.png); }"),
            (PAGE.to_string(), "@import 'x.css';"),
        ] {
            assert!(!inspect(&draft(&index, css)).is_empty(), "{index} {css}");
        }
        // 文字中出现 on 或 “one=” 之类的写法不算事件属性。
        assert!(!has_event_attribute("<p>turn on = off</p>"));
        assert!(!has_event_attribute("<p class=\"button\">x</p>"));
        assert!(has_event_attribute("<p\nonload=x>"));
    }

    #[test]
    fn probe_script_is_built_only_from_validated_identifiers_and_results_are_parsed() {
        let page = with_probe_script(PAGE, &draft(PAGE, ""));
        assert!(page.contains("[\"title\",\"fontSize\"]"));
        assert!(page.find("<script>").unwrap() < page.find("</body>").unwrap());
        let dom = "<html><body><pre id=\"eve-probe-results\">{\"ua\":\"Mozilla/5.0 HeadlessChrome/141.0.7390.37 Safari/537.36\",\"sheets\":[{\"href\":\"style.css\",\"loaded\":true}],\"probes\":[\"a &amp; b &lt;c&gt;\"]}</pre></body></html>";
        let parsed = results(dom).unwrap();
        assert_eq!(parsed.probes, vec![Some("a & b <c>".to_string())]);
        assert_eq!(version(&parsed.ua), "Chromium 141.0.7390.37 (headless)");
        assert!(same("rgb(200, 30, 30)", "rgb(200,30,30)") && !same("40px", "41px"));
        assert!(
            file_url(Path::new("/tmp/a b/eve-run.html"))
                .unwrap()
                .ends_with("/tmp/a%20b/eve-run.html")
        );
    }
}
