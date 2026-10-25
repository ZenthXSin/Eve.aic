//! 继承自 eve-portable 的 Mindustry 模组验证器（`vendor/mindustry-mod-validator`）作为实践的附加阶段：
//! 服务端确认加载后，再用它放置模组方块、生成模组单位并跑更新帧，把运行时崩溃与内容错误计入证据。
//! 验证器在独立进程中运行，只拿到草稿所在目录与一个临时目录；它不是沙箱，只用于已通过结构检查的纯数据草稿。
use crate::RuntimeCommand;
use ring::digest::{Context, SHA256};
use serde_json::Value;
use std::{io::Read, path::Path, path::PathBuf, process::Stdio, time::Duration};
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(180);

pub struct ModValidator {
    command: RuntimeCommand,
    jar: PathBuf,
    sha256: String,
}

impl ModValidator {
    pub fn new(command: RuntimeCommand, jar: PathBuf) -> Result<Self, String> {
        let mut file = std::fs::File::open(&jar).map_err(|_| "无法读取模组验证器 jar")?;
        let mut digest = Context::new(&SHA256);
        let mut buffer = vec![0; 64 * 1024];
        let mut first = true;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| "无法读取模组验证器 jar")?;
            if read == 0 {
                break;
            }
            if first && !buffer[..read].starts_with(b"PK") {
                return Err("模组验证器必须是 jar 文件".into());
            }
            first = false;
            digest.update(&buffer[..read]);
        }
        if first {
            return Err("模组验证器 jar 为空".into());
        }
        Ok(Self {
            command,
            jar,
            sha256: crate::hex(digest.finish().as_ref()),
        })
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// 验证 `mod_dir` 中的模组，返回错误级别的问题（空表示通过）；验证器自身无法给出报告时返回 Err。
    /// `scratch` 是供验证器写临时数据与报告的空目录。
    pub async fn check(&self, mod_dir: &Path, scratch: &Path) -> Result<Vec<String>, String> {
        let report = scratch.join("report.json");
        let mut command = Command::new(&self.command.program);
        command
            .args(&self.command.prefix_args)
            .args(["-Xmx1g", "-Djava.awt.headless=true"])
            .arg(format!("-Djava.io.tmpdir={}", scratch.display()))
            .arg("-jar")
            .arg(&self.jar)
            .arg(mod_dir)
            .args(["--json", "--output"])
            .arg(&report)
            .current_dir(scratch)
            .env_clear();
        for name in ["PATH", "SystemRoot", "SYSTEMROOT", "JAVA_HOME"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for name in ["HOME", "USERPROFILE", "TEMP", "TMP"] {
            command.env(name, scratch);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|_| "无法启动模组验证器".to_string())?;
        let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "模组验证器超时".to_string())?
            .map_err(|_| "模组验证器异常退出".to_string())?;
        let code = output.status.code();
        let text = std::fs::read_to_string(&report).ok();
        match (code, text) {
            (Some(0 | 1), Some(text)) => parse(&text),
            _ => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let tail: String = stderr
                    .lines()
                    .rev()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect();
                Err(format!("模组验证器未给出报告（退出码 {code:?}）{tail}"))
            }
        }
    }
}

/// 验证器的 JSON 报告不转义字符串里的制表符与回车；先换成空格再解析（结构外的空白不受影响）。
fn parse(text: &str) -> Result<Vec<String>, String> {
    let cleaned: String = text
        .chars()
        .map(|c| if c == '\t' || c == '\r' { ' ' } else { c })
        .collect();
    let value: Value =
        serde_json::from_str(&cleaned).map_err(|_| "模组验证器报告无法解析".to_string())?;
    let issues = value
        .get("issues")
        .and_then(Value::as_array)
        .ok_or("模组验证器报告缺少 issues")?;
    Ok(issues
        .iter()
        .filter(|issue| {
            matches!(
                issue.get("severity").and_then(Value::as_str),
                Some("ERROR" | "CRITICAL")
            )
        })
        .map(|issue| {
            let text = |key| issue.get(key).and_then(Value::as_str).unwrap_or_default();
            format!("验证器 {}：{}", text("category"), text("message"))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_with_raw_tabs_keeps_only_errors() {
        let report = "{\n  \"status\": \"失败\",\n  \"issues\": [\n    {\"severity\": \"WARN\", \"category\": \"runtime-log\", \"message\": \"a\"},\n    {\"severity\": \"ERROR\", \"category\": \"block-test\", \"message\": \"方块 'x' 更新时崩溃:\tNPE\"},\n    {\"severity\": \"CRITICAL\", \"category\": \"load\", \"message\": \"栈溢出\"}\n  ]\n}\n";
        assert_eq!(
            parse(report).unwrap(),
            vec![
                "验证器 block-test：方块 'x' 更新时崩溃: NPE".to_string(),
                "验证器 load：栈溢出".to_string()
            ]
        );
        assert!(parse("{}").is_err() && parse("not json").is_err());
    }
}
