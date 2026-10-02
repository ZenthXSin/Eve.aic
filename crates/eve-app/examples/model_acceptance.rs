//! 显式运行配置好的主模型；CI 使用相同验收流程的本地 HTTP 测试。
#[path = "support/model_check.rs"]
mod model_check;
use model_check::{CheckError, CheckOptions};
use std::{
    path::PathBuf,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const HELP: &str = "主模型多轮验收
用法：model_acceptance <全新验收目录> [--eve 二进制] [--agent AGENT.md]
先 cargo build -p eve-app --bin eve --locked，再运行本示例。
使用 EVE_OPENAI_MODEL/PROTOCOL/API_KEY/BASE_URL 等核心入口环境配置。\n默认 deepseek-v4.1-flash / chat；凭据仅由宿主提供。
执行两轮聊天和一次工具调用，再以新进程恢复第三轮；成功路径四次 Provider 请求。
只打印脱敏 JSON 报告；原始进程输出和状态保留在指定目录。";

fn main() {
    if let Err(error) = execute() {
        println!("{}", error.report());
        std::process::exit(1);
    }
}
fn execute() -> Result<(), CheckError> {
    let mut args = std::env::args_os().skip(1);
    let Some(directory) = args.next() else {
        println!("{HELP}");
        return Err(CheckError {
            stage: "preflight",
            code: "arguments",
            report_write_failed: false,
        });
    };
    if directory == "--help" || directory == "-h" {
        println!("{HELP}");
        return Ok(());
    }
    let fail = |code| CheckError {
        stage: "preflight",
        code,
        report_write_failed: false,
    };
    if directory.is_empty() || directory.to_str().is_some_and(|s| s.starts_with("--")) {
        return Err(fail("arguments"));
    }
    let executable = std::env::current_exe().map_err(|_| fail("binary_path"))?;
    let mut eve = executable
        .parent()
        .and_then(|p| p.parent())
        .ok_or_else(|| fail("binary_path"))?
        .join(format!("eve{}", std::env::consts::EXE_SUFFIX));
    let mut agent = PathBuf::from("AGENT.md");
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| fail("arguments"))?;
        match flag.to_str() {
            Some("--eve") => eve = value.into(),
            Some("--agent") => agent = value.into(),
            _ => return Err(fail("arguments")),
        }
    }
    if std::env::var("EVE_OPENAI_API_KEY")
        .ok()
        .is_none_or(|v| v.trim().is_empty())
    {
        return Err(fail("model_or_credential_missing"));
    }
    eve_agent_prompt::FileAgentPrompt::new(&agent).map_err(|_| fail("agent_invalid"))?;
    if !eve.is_file() {
        return Err(fail("binary_missing"));
    }
    let timeout = match std::env::var("EVE_OPENAI_TIMEOUT_SECONDS") {
        Ok(value) => value
            .parse::<u64>()
            .ok()
            .filter(|v| (1..=600).contains(v))
            .ok_or_else(|| fail("timeout_invalid"))?,
        Err(std::env::VarError::NotPresent) => 120,
        Err(_) => return Err(fail("timeout_invalid")),
    };
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| fail("clock"))?
        .as_nanos();
    let marker = format!("EVE_CORE_{}_{stamp}", std::process::id());
    let options = CheckOptions {
        directory: directory.into(),
        agent_path: agent,
        request_timeout: Duration::from_secs(timeout),
    };
    let report = model_check::run(&options, &marker, || Command::new(&eve))?;
    println!("{report}");
    Ok(())
}
