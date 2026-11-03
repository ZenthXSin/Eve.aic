//! 小型工具编排程序复用已有 OS 沙箱和审计边界，不另起一套权限系统。
use crate::{Language, Result, ScriptSpec, StateSchema, archive_digest, reject};
use std::collections::BTreeMap;

pub fn tool_code_spec(code: &str, tools: Vec<String>) -> Result<ScriptSpec> {
    if code.trim().is_empty() || code.len() > 32 * 1024 {
        return Err(reject("工具代码需为1至32768字节"));
    }
    let code = serde_json::to_string(code).map_err(reject)?;
    let tools_json = serde_json::to_string(&tools).map_err(reject)?;
    let source = format!("{}\nmain({code},{tools_json});\n", include_str!("code.mjs"));
    let spec = ScriptSpec {
        name: format!("tool-code-{}", archive_digest(source.as_bytes())),
        language: Language::Javascript,
        source,
        tools,
        dependencies: BTreeMap::new(),
        state_schema: StateSchema {
            version: 1,
            fields: BTreeMap::new(),
        },
        initial_state: serde_json::json!({}),
    };
    spec.validate()?;
    Ok(spec)
}
