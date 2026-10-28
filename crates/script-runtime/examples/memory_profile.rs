//! 固定脚本账本样本的进程RSS测量；仅本地临时目录，不执行脚本或外部工具。
//! 用 /usr/bin/time -v 记录峰值，比较同一debug配置下的修前/修后产物。
use eve_llm_api::ContextScope;
use eve_script_runtime::{Language, ScriptSpec, ScriptStore, StateSchema};
use serde_json::json;
use std::collections::BTreeMap;
fn rss() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}
fn phase(name: &str, path: &std::path::Path) {
    println!(
        "{}",
        json!({"phase":name,"rss_kib":rss(),"ledger_bytes":std::fs::metadata(path.join("ledger.json")).ok().map(|m|m.len())})
    );
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let store = ScriptStore::open(root.path())?;
    phase("empty", root.path());
    for i in 0..64 {
        let owner = ContextScope {
            session_id: format!("scope-{}", i % 8),
            user_id: format!("owner-{}", i % 8),
        };
        let published = store.publish(
            &owner,
            ScriptSpec {
                name: "profile".into(),
                language: Language::Python,
                source: format!("# version {i}\n# {}", "x".repeat(128 * 1024)),
                dependencies: BTreeMap::new(),
                tools: vec![],
                state_schema: StateSchema {
                    version: 1,
                    fields: BTreeMap::new(),
                },
                initial_state: json!({}),
            },
        )?;
        store.activate(&owner, &published.reference, None)?;
        if (i + 1) % 16 == 0 {
            phase(&format!("versions-{}", i + 1), root.path());
        }
    }
    drop(store);
    phase("closed", root.path());
    let reopened = ScriptStore::open(root.path())?;
    phase("reopened", root.path());
    assert_eq!(
        reopened
            .list(&ContextScope {
                session_id: "scope-0".into(),
                user_id: "owner-0".into()
            })?
            .len(),
        8
    );
    Ok(())
}
