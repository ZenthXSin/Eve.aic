//! 常驻 Task 仅用于目录与调度；完整任务必须按不可变引用核对后读取。
use crate::{
    archive::{key, valid},
    store::{self, Ledger},
    strict_json,
};
use eve_task_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    hash: String,
    bytes: usize,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    directory: Task,
    record: Record,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format_version: u32,
    tasks: Vec<Entry>,
}
#[derive(Clone)]
pub(super) struct ColdState {
    archive: Arc<dyn TaskArchiveStore>,
    records: BTreeMap<String, Record>,
    active: Arc<AtomicBool>,
}
pub(super) struct Prepared {
    pub bytes: Vec<u8>,
    pub tasks: Vec<Task>,
    pub state: ColdState,
}

fn directory(task: &Task) -> Task {
    Task {
        id: task.id.clone(),
        owner: task.owner.clone(),
        source: task.source.clone(),
        spec: TaskSpec {
            title: task.spec.title.clone(),
            instructions: task.spec.instructions.clone(),
            max_steps: task.spec.max_steps,
            context: None,
        },
        created_at_ms: task.created_at_ms,
        status: task.status,
        cancel_requested: task.cancel_requested,
        started_at_ms: task.started_at_ms,
        finished_at_ms: task.finished_at_ms,
        usage: task.usage,
        result: None,
        failure: None,
        notice: task.notice.clone(),
        execution: task.execution.as_ref().map(|execution| TaskExecution {
            checkpoint: None,
            deadline_at_ms: execution.deadline_at_ms,
            authorization_deadline_at_ms: execution.authorization_deadline_at_ms,
            recoveries: execution.recoveries,
            resume_at_ms: execution.resume_at_ms,
        }),
    }
}
impl ColdState {
    pub(super) fn compact(&self) {
        // 正文不可变、当前索引已确认；回收失败不撤销已提交业务结果，put仍受物理配额保护。
        let _ = self.archive.compact(
            &self
                .records
                .values()
                .map(|record| record.hash.clone())
                .collect::<Vec<_>>(),
        );
    }
    pub(super) fn empty(archive: Arc<dyn TaskArchiveStore>) -> Self {
        Self {
            archive,
            records: BTreeMap::new(),
            active: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(super) fn restore(
        archive: Arc<dyn TaskArchiveStore>,
        value: serde_json::Value,
    ) -> TaskResult<(Ledger, Self)> {
        let index: Index = serde_json::from_value(value).map_err(|_| TaskError::CorruptState)?;
        if index.format_version != 4 || index.tasks.len() > MAX_TASKS {
            return Err(TaskError::CorruptState);
        }
        let mut state = Self::empty(archive);
        let mut tasks = Vec::new();
        let mut ids = BTreeSet::new();
        for entry in index.tasks {
            if !ids.insert(entry.directory.id.clone())
                || !valid(&entry.record.hash)
                || entry.record.bytes > MAX_STATE_BYTES
            {
                return Err(TaskError::CorruptState);
            }
            state
                .records
                .insert(entry.directory.id.clone(), entry.record);
            // 全表核验先于恢复写入；每次只持有一条完整任务。
            state.load(&entry.directory)?;
            tasks.push(entry.directory);
        }
        state
            .check_capacity(&tasks.iter().collect::<Vec<_>>())
            .map_err(|_| TaskError::CorruptState)?;
        Ok((
            Ledger {
                format_version: 4,
                tasks,
            },
            state,
        ))
    }
    pub(super) fn load(&self, metadata: &Task) -> TaskResult<Task> {
        if !self.active() {
            return Err(TaskError::Unavailable);
        }
        let result = self.load_inner(metadata);
        if matches!(result, Err(TaskError::CorruptState | TaskError::Storage)) {
            self.active.store(false, Ordering::SeqCst);
        }
        result
    }
    pub(super) fn active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }
    fn load_inner(&self, metadata: &Task) -> TaskResult<Task> {
        let record = self
            .records
            .get(&metadata.id)
            .ok_or(TaskError::CorruptState)?;
        let bytes = self.archive.get(&record.hash)?;
        if bytes.len() != record.bytes || key(&bytes) != record.hash {
            return Err(TaskError::CorruptState);
        }
        let task: Task = serde_json::from_value(
            strict_json::from_slice(&bytes).map_err(|_| TaskError::CorruptState)?,
        )
        .map_err(|_| TaskError::CorruptState)?;
        store::validate_task(&task).map_err(|_| TaskError::CorruptState)?;
        if directory(&task) != *metadata {
            return Err(TaskError::CorruptState);
        }
        Ok(task)
    }
    fn check_capacity(&self, tasks: &[&Task]) -> TaskResult<()> {
        let mut length = b"{\"format_version\":2,\"tasks\":[]}"
            .len()
            .saturating_add(tasks.len().saturating_sub(1));
        for task in tasks {
            length = length
                .checked_add(
                    self.records
                        .get(&task.id)
                        .ok_or(TaskError::CorruptState)?
                        .bytes,
                )
                .ok_or(TaskError::LimitReached)?;
        }
        if tasks.len() > MAX_TASKS || length > store::available_capacity(tasks)? {
            return Err(TaskError::LimitReached);
        }
        Ok(())
    }
    /// 只有 changed 指定的完整正文被写入；其余目录不能冒充完整任务重新冻结。
    pub(super) fn prepare(&self, tasks: &[&Task], changed: Option<&str>) -> TaskResult<Prepared> {
        if !self.active() {
            return Err(TaskError::Unavailable);
        }
        let mut state = self.clone();
        let mut frozen = Vec::new();
        let mut ids = BTreeSet::new();
        let mut write = Vec::new();
        for task in tasks {
            if !ids.insert(task.id.clone()) {
                return Err(TaskError::CorruptState);
            }
            if changed == Some(task.id.as_str()) || !state.records.contains_key(&task.id) {
                store::validate_task(task).map_err(|_| TaskError::InvalidInput)?;
                let bytes = store::bounded_encode(task, MAX_STATE_BYTES)?;
                let hash = key(&bytes);
                state.records.insert(
                    task.id.clone(),
                    Record {
                        hash: hash.clone(),
                        bytes: bytes.len(),
                    },
                );
                write.push((hash, bytes));
            }
            frozen.push(directory(task));
        }
        state.records.retain(|id, _| ids.contains(id));
        state.check_capacity(tasks)?;
        let entries = frozen
            .iter()
            .map(|task| Entry {
                directory: task.clone(),
                record: state.records[&task.id].clone(),
            })
            .collect();
        let bytes = store::bounded_encode(
            &Index {
                format_version: 4,
                tasks: entries,
            },
            MAX_STATE_BYTES,
        )?;
        for (hash, body) in write {
            if let Err(error) = state.archive.put(&hash, &body) {
                if matches!(error, TaskError::Storage | TaskError::CorruptState) {
                    self.active.store(false, Ordering::SeqCst);
                }
                return Err(error);
            }
        }
        Ok(Prepared {
            bytes,
            tasks: frozen,
            state,
        })
    }
}
