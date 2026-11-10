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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    directory: Task,
    record: TaskArchiveRecord,
    /// 格式 4 必须缺省，格式 5 必须显式保存；不是删除记录或完成任务。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived: Option<bool>,
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
    records: BTreeMap<String, TaskArchiveRecord>,
    closeout: BTreeMap<String, usize>,
    archived: BTreeSet<String>,
    format_version: u32,
    active: Arc<AtomicBool>,
}
pub(super) struct Prepared {
    pub bytes: Vec<u8>,
    pub tasks: Vec<Task>,
    pub state: ColdState,
}

pub(super) fn directory(task: &Task) -> Task {
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
            kind: execution.kind,
            session_end_reason: execution.session_end_reason,
            exploration_acknowledged_at_ms: execution.exploration_acknowledged_at_ms,
            checkpoint: None,
            last_finished_at_ms: execution.last_finished_at_ms,
            progress_notices: execution.progress_notices.clone(),
            // 授权原话只留在按摘要核对的完整冷正文中，目录不复制用户正文。
            review_messages: vec![],
            completion_contract: execution.completion_contract.clone(),
            frozen_node_evidence: vec![],
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
            closeout: BTreeMap::new(),
            archived: BTreeSet::new(),
            format_version: 4,
            active: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(super) fn restore(
        archive: Arc<dyn TaskArchiveStore>,
        value: serde_json::Value,
    ) -> TaskResult<(Ledger, Self)> {
        let version = value.get("format_version").and_then(|v| v.as_u64());
        let entries = value
            .get("tasks")
            .and_then(|v| v.as_array())
            .ok_or(TaskError::CorruptState)?;
        if entries.iter().any(|entry| match version {
            Some(4) => entry.get("archived").is_some(),
            Some(5) => entry.get("archived").and_then(|v| v.as_bool()).is_none(),
            _ => true,
        }) {
            return Err(TaskError::CorruptState);
        }
        let index: Index = serde_json::from_value(value).map_err(|_| TaskError::CorruptState)?;
        if !matches!(index.format_version, 4 | 5) || index.tasks.len() > MAX_TASKS {
            return Err(TaskError::CorruptState);
        }
        let mut state = Self::empty(archive);
        state.format_version = index.format_version;
        let mut tasks = Vec::new();
        let mut ids = BTreeSet::new();
        for entry in index.tasks {
            if !ids.insert(entry.directory.id.clone())
                || !valid(&entry.record.hash)
                || entry.record.bytes == 0
                || entry.record.bytes > MAX_STATE_BYTES
                || (index.format_version == 4 && entry.archived.is_some())
                || (index.format_version == 5 && entry.archived.is_none())
                || (entry.archived == Some(true) && entry.directory.settled_at().is_none())
            {
                return Err(TaskError::CorruptState);
            }
            if entry.archived == Some(true) {
                state.archived.insert(entry.directory.id.clone());
            }
            state
                .records
                .insert(entry.directory.id.clone(), entry.record);
            // 全表核验先于恢复写入；每次只持有一条完整任务。
            let task = state.load(&entry.directory)?;
            state.closeout.insert(
                task.id.clone(),
                crate::capacity::closeout_maximum(&task, state.records[&task.id].bytes)?,
            );
            tasks.push(entry.directory);
        }
        state
            .check_capacity(&tasks.iter().collect::<Vec<_>>())
            .map_err(|_| TaskError::CorruptState)?;
        Ok((
            Ledger {
                format_version: index.format_version,
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
    fn check_total_capacity(&self, tasks: &[&Task]) -> TaskResult<()> {
        if tasks.len() > MAX_TASKS {
            return Err(TaskError::LimitReached);
        }
        let mut total = 0usize;
        for task in tasks {
            total = total
                .checked_add(
                    self.records
                        .get(&task.id)
                        .ok_or(TaskError::CorruptState)?
                        .bytes,
                )
                .ok_or(TaskError::LimitReached)?;
        }
        if total > MAX_ARCHIVE_BYTES {
            return Err(TaskError::LimitReached);
        }
        Ok(())
    }
    fn check_working_capacity(&self, tasks: &[&Task]) -> TaskResult<()> {
        let count = tasks
            .iter()
            .filter(|task| !self.archived.contains(&task.id))
            .count();
        let mut length = b"{\"format_version\":2,\"tasks\":[]}"
            .len()
            .saturating_add(count.saturating_sub(1));
        for task in tasks {
            if self.archived.contains(&task.id) {
                if task.settled_at().is_none() {
                    return Err(TaskError::CorruptState);
                }
                continue;
            }
            length = length
                .checked_add(
                    self.records
                        .get(&task.id)
                        .ok_or(TaskError::CorruptState)?
                        .bytes,
                )
                .ok_or(TaskError::LimitReached)?;
        }
        // 原收尾预留继续按完整目录计算，归档不隐藏通知或进度回执。
        if length > store::available_capacity(tasks)? {
            return Err(TaskError::LimitReached);
        }
        Ok(())
    }
    fn check_closeout_capacity(&self, tasks: &[&Task]) -> TaskResult<()> {
        // 保留原工作集下界，同时保证所有未结算任务增长到最大正文后仍能收尾。
        self.check_working_capacity(tasks)?;
        let count = tasks
            .iter()
            .filter(|task| !self.archived.contains(&task.id))
            .count();
        let mut length = b"{\"format_version\":2,\"tasks\":[]}"
            .len()
            .saturating_add(count.saturating_sub(1));
        for task in tasks {
            if self.archived.contains(&task.id) {
                continue;
            }
            let bytes = if task.settled_at().is_some() {
                self.records[&task.id].bytes
            } else {
                *self.closeout.get(&task.id).ok_or(TaskError::CorruptState)?
            };
            if bytes > MAX_STATE_BYTES {
                return Err(TaskError::LimitReached);
            }
            length = length.checked_add(bytes).ok_or(TaskError::LimitReached)?;
        }
        if length > store::available_capacity(tasks)? {
            return Err(TaskError::LimitReached);
        }
        Ok(())
    }
    fn physical_reserve(&self, tasks: &[&Task]) -> TaskResult<(usize, usize)> {
        let mut bytes = 0usize;
        let mut files = 0usize;
        for task in tasks.iter().filter(|task| task.settled_at().is_none()) {
            let current = self
                .records
                .get(&task.id)
                .ok_or(TaskError::CorruptState)?
                .bytes;
            let maximum = *self.closeout.get(&task.id).ok_or(TaskError::CorruptState)?;
            // (M-c) 是未来稳态增长，M 是整批重启的完整替换文件；正文增长时预留等额下降。
            bytes = maximum
                .checked_mul(2)
                .and_then(|twice| twice.checked_sub(current))
                .and_then(|reserve| bytes.checked_add(reserve))
                .ok_or(TaskError::LimitReached)?;
            files += 1;
        }
        Ok((bytes, files))
    }
    fn check_capacity(&self, tasks: &[&Task]) -> TaskResult<()> {
        self.check_total_capacity(tasks)?;
        self.check_working_capacity(tasks)
    }
    fn archive_to_fit(&mut self, tasks: &[&Task]) -> TaskResult<()> {
        self.check_total_capacity(tasks)?;
        match self.check_closeout_capacity(tasks) {
            Ok(()) => return Ok(()),
            Err(TaskError::LimitReached) => {}
            Err(error) => return Err(error),
        }
        let mut candidates: Vec<_> = tasks
            .iter()
            .filter(|task| !self.archived.contains(&task.id))
            .filter_map(|task| task.settled_at().map(|at| (at, task.id.clone())))
            .collect();
        candidates.sort_unstable();
        for (_, id) in candidates {
            self.archived.insert(id);
            self.format_version = 5;
            match self.check_closeout_capacity(tasks) {
                Ok(()) => return Ok(()),
                Err(TaskError::LimitReached) => {}
                Err(error) => return Err(error),
            }
        }
        Err(TaskError::LimitReached)
    }
    /// 只有 changed 指定的完整正文被写入；其余目录不能冒充完整任务重新冻结。
    pub(super) fn prepare(&self, tasks: &[&Task], changed: Option<&str>) -> TaskResult<Prepared> {
        self.prepare_many(tasks, &changed.into_iter().collect::<Vec<_>>())
    }
    /// 恢复与迁移可一次替换多条；所有容量预检先于本批的第一份正文写入。
    pub(super) fn prepare_many(&self, tasks: &[&Task], changed: &[&str]) -> TaskResult<Prepared> {
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
            if changed.contains(&task.id.as_str()) || !state.records.contains_key(&task.id) {
                // 修改或复活先重新计入运行工作集；不以旧归档位绕过容量。
                state.archived.remove(&task.id);
                store::validate_task(task).map_err(|_| TaskError::InvalidInput)?;
                let bytes = store::bounded_encode(task, MAX_STATE_BYTES)?;
                let hash = key(&bytes);
                state.closeout.insert(
                    task.id.clone(),
                    crate::capacity::closeout_maximum(task, bytes.len())?,
                );
                state.records.insert(
                    task.id.clone(),
                    TaskArchiveRecord {
                        hash: hash.clone(),
                        bytes: bytes.len(),
                    },
                );
                write.push((hash, bytes));
            }
            frozen.push(directory(task));
        }
        state.records.retain(|id, _| ids.contains(id));
        state.closeout.retain(|id, _| ids.contains(id));
        state.archived.retain(|id| ids.contains(id));
        // 只在克隆候选中选择归档。整个工作集无法容纳时不写正文、不提交归档位。
        state.archive_to_fit(tasks)?;
        let entries = frozen
            .iter()
            .map(|task| Entry {
                directory: task.clone(),
                record: state.records[&task.id].clone(),
                archived: (state.format_version == 5).then(|| state.archived.contains(&task.id)),
            })
            .collect();
        let bytes = store::bounded_encode(
            &Index {
                format_version: state.format_version,
                tasks: entries,
            },
            MAX_STATE_BYTES,
        )?;
        let (reserve_bytes, reserve_files) = state.physical_reserve(tasks)?;
        let current: Vec<_> = self.records.values().cloned().collect();
        let retained: Vec<_> = state.records.values().cloned().collect();
        let writes: Vec<_> = write
            .iter()
            .map(|(hash, body)| TaskArchiveRecord {
                hash: hash.clone(),
                bytes: body.len(),
            })
            .collect();
        let plan = TaskArchivePlan {
            current: &current,
            retained: &retained,
            writes: &writes,
            reserve_bytes,
            reserve_files,
        };
        let mut capacity = state.archive.check_capacity(&plan);
        if matches!(capacity, Err(TaskError::LimitReached)) {
            // 上次索引可能已落盘，而崩溃/回收失败留下了退役正文。只按已经确认的
            // 当前索引验证并补回收，再核对整个候选；绝不能提前删除本次将退役的正文。
            capacity = self
                .archive
                .compact(
                    &current
                        .iter()
                        .map(|record| record.hash.clone())
                        .collect::<Vec<_>>(),
                )
                .and_then(|()| state.archive.check_capacity(&plan));
        }
        if let Err(error) = capacity {
            if matches!(error, TaskError::Storage | TaskError::CorruptState) {
                self.active.store(false, Ordering::SeqCst);
            }
            return Err(error);
        }
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
