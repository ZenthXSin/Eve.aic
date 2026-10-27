use crate::{SavedWorkflow, WorkflowDefinition, digest, failed};
use eve_llm_api::{ContextScope, Tool, ToolExecutionError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

const MAX_WORKFLOWS: usize = 128;
const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    next_run: u64,
    workflows: Vec<SavedWorkflow>,
}
pub struct WorkflowStore {
    directory: PathBuf,
    ledger: Mutex<Ledger>,
    pub(crate) leaves: OnceLock<BTreeMap<String, Arc<dyn Tool>>>,
}
impl WorkflowStore {
    pub fn open(directory: PathBuf) -> Result<Arc<Self>, ToolExecutionError> {
        std::fs::create_dir_all(directory.join("runs")).map_err(failed)?;
        let path = directory.join("workflows.json");
        let ledger = match std::fs::read(path) {
            Ok(bytes) if bytes.len() <= MAX_STATE_BYTES => serde_json::from_slice::<Ledger>(&bytes)
                .map_err(|_| failed("流程账本损坏；保留原文件，拒绝清空"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger {
                version: 1,
                next_run: 1,
                workflows: vec![],
            },
            _ => return Err(failed("流程账本无法读取或超过容量；原文件保留")),
        };
        if ledger.version != 1 || ledger.next_run == 0 || ledger.workflows.len() > MAX_WORKFLOWS {
            return Err(failed("流程账本版本或结构无效；保留原文件"));
        }
        let mut ids = std::collections::HashSet::new();
        for saved in &ledger.workflows {
            saved.definition.validate()?;
            if saved.id != workflow_id(&saved.owner, &saved.definition.name)?
                || saved.revision == 0
                || !ids.insert(&saved.id)
            {
                return Err(failed("流程账本身份或版本无效；保留原文件"));
            }
        }
        Ok(Arc::new(Self {
            directory,
            ledger: Mutex::new(ledger),
            leaves: OnceLock::new(),
        }))
    }
    /// 只能由组合层注入已通过身份包装的工具；不接纳模型提供的实现。
    pub fn bind_tools(&self, leaves: Vec<Arc<dyn Tool>>) -> Result<(), ToolExecutionError> {
        let map = leaves
            .into_iter()
            .map(|t| (t.definition().name, t))
            .collect();
        self.leaves
            .set(map)
            .map_err(|_| failed("流程叶子工具不可重复绑定"))
    }
    pub(crate) fn save(
        &self,
        owner: &ContextScope,
        definition: WorkflowDefinition,
        expected: Option<u64>,
    ) -> Result<SavedWorkflow, ToolExecutionError> {
        definition.validate()?;
        let mut guard = self.ledger.lock().map_err(|_| failed("流程账本锁不可用"))?;
        let mut next = guard.clone();
        let id = workflow_id(owner, &definition.name)?;
        let saved = if let Some(existing) = next.workflows.iter_mut().find(|w| w.id == id) {
            if existing.definition == definition {
                return Ok(existing.clone());
            }
            if expected != Some(existing.revision) {
                return Err(failed(
                    "同名流程已有不同内容；先读取版本，再用 expected_revision 更新",
                ));
            }
            existing.revision = existing
                .revision
                .checked_add(1)
                .ok_or_else(|| failed("流程版本已用尽"))?;
            existing.definition = definition;
            existing.clone()
        } else {
            if expected.is_some_and(|n| n != 0) || next.workflows.len() >= MAX_WORKFLOWS {
                return Err(failed("流程不存在或账本已满；原记录保留"));
            }
            let saved = SavedWorkflow {
                id,
                revision: 1,
                owner: owner.clone(),
                definition,
            };
            next.workflows.push(saved.clone());
            saved
        };
        atomic(
            &self.directory.join("workflows.json"),
            &next,
            MAX_STATE_BYTES,
        )?;
        *guard = next;
        Ok(saved)
    }
    pub(crate) fn list(
        &self,
        owner: &ContextScope,
    ) -> Result<Vec<SavedWorkflow>, ToolExecutionError> {
        Ok(self
            .ledger
            .lock()
            .map_err(|_| failed("流程账本锁不可用"))?
            .workflows
            .iter()
            .filter(|w| &w.owner == owner)
            .cloned()
            .collect())
    }
    pub(crate) fn get(
        &self,
        owner: &ContextScope,
        id: &str,
    ) -> Result<SavedWorkflow, ToolExecutionError> {
        self.list(owner)?
            .into_iter()
            .find(|w| w.id == id)
            .ok_or_else(|| failed("当前会话没有该流程"))
    }
    pub(crate) fn begin_run(&self) -> Result<String, ToolExecutionError> {
        let mut guard = self.ledger.lock().map_err(|_| failed("流程账本锁不可用"))?;
        let mut next = guard.clone();
        if next.next_run > 4096 {
            return Err(failed("流程执行记录已满；旧记录保留，归档前不接受新执行"));
        }
        let id = format!("workflow-run-{}", next.next_run);
        next.next_run = next
            .next_run
            .checked_add(1)
            .ok_or_else(|| failed("流程执行编号已用尽"))?;
        atomic(
            &self.directory.join("workflows.json"),
            &next,
            MAX_STATE_BYTES,
        )?;
        *guard = next;
        Ok(id)
    }
    pub(crate) fn write_run(&self, id: &str, value: &Value) -> Result<(), ToolExecutionError> {
        atomic(
            &self.directory.join("runs").join(format!("{id}.json")),
            value,
            1024 * 1024,
        )
    }
    pub(crate) fn read_run(
        &self,
        owner: &ContextScope,
        id: &str,
    ) -> Result<Value, ToolExecutionError> {
        if !id
            .strip_prefix("workflow-run-")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(failed("流程执行编号无效"));
        }
        let bytes = std::fs::read(self.directory.join("runs").join(format!("{id}.json")))
            .map_err(|_| failed("流程执行记录不存在或不可读"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(failed("流程执行记录超过上限"));
        }
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| failed("流程执行记录损坏；原文件保留"))?;
        if value.get("owner") != Some(&serde_json::to_value(owner).map_err(failed)?) {
            return Err(failed("不能读取其他会话的流程执行记录"));
        }
        if value["status"] == "running" {
            value["note"] = Value::String(
                "尚无最终确认，可能仍在执行或已经中断；核对真实状态，不自动重放".into(),
            );
        }
        Ok(value)
    }
}
fn workflow_id(owner: &ContextScope, name: &str) -> Result<String, ToolExecutionError> {
    Ok(format!("workflow-{}", digest(&(owner, name))?))
}
fn atomic(path: &Path, value: &impl Serialize, limit: usize) -> Result<(), ToolExecutionError> {
    let bytes = serde_json::to_vec(value).map_err(failed)?;
    if bytes.len() > limit {
        return Err(failed("流程记录超过容量；原记录保留"));
    }
    let parent = path.parent().ok_or_else(|| failed("流程存储路径无效"))?;
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| failed("流程临时文件不可写"))?;
    staged
        .write_all(&bytes)
        .map_err(|_| failed("流程记录写入失败"))?;
    staged
        .as_file()
        .sync_all()
        .map_err(|_| failed("流程记录同步失败"))?;
    staged
        .persist(path)
        .map_err(|_| failed("流程记录提交失败；原文件保留"))?;
    Ok(())
}
