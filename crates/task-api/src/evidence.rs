//! 明确指定范围的任务证据视图；不是全局 TaskSnapshot，不用于全局调度或通知。
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEvidenceKey {
    pub id: String,
    pub owner: TaskOwner,
}

/// 查询未请求的任务必须报不可用，不能把没读取的来源当作已经回收。
pub trait TaskEvidenceReader: Send + Sync {
    fn find(&self, id: &str) -> TaskResult<Option<&Task>>;
}
impl TaskEvidenceReader for TaskSnapshot {
    fn find(&self, id: &str) -> TaskResult<Option<&Task>> {
        Ok(self.get(id))
    }
}

pub struct TaskEvidenceSnapshot {
    requested: BTreeMap<String, TaskOwner>,
    tasks: BTreeMap<String, Task>,
}
impl TaskEvidenceSnapshot {
    pub(crate) fn keys(keys: &[TaskEvidenceKey]) -> TaskResult<BTreeMap<String, TaskOwner>> {
        if keys.len() > MAX_TASK_STRATEGY_RECORDS {
            return Err(TaskError::LimitReached);
        }
        let mut requested = BTreeMap::new();
        for key in keys {
            validate_id(&key.id)?;
            validate_owner(&key.owner)?;
            if let Some(previous) = requested.insert(key.id.clone(), key.owner.clone())
                && previous != key.owner
            {
                return Err(TaskError::CorruptState);
            }
        }
        Ok(requested)
    }
    /// 调用者须在同一不可变账本视图下调用；只复制命中的完整记录。
    /// 全部元数据仍核对唯一性，身份替换不能冒充来源已被回收。
    pub fn select(tasks: &[Task], keys: &[TaskEvidenceKey]) -> TaskResult<Self> {
        let requested = Self::keys(keys)?;
        if tasks.len() > MAX_TASKS {
            return Err(TaskError::CorruptState);
        }
        let mut selected = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for task in tasks {
            validate_id(&task.id).map_err(|_| TaskError::CorruptState)?;
            validate_owner(&task.owner).map_err(|_| TaskError::CorruptState)?;
            if !ids.insert(&task.id) {
                return Err(TaskError::CorruptState);
            }
            if let Some(owner) = requested.get(&task.id) {
                if owner != &task.owner {
                    return Err(TaskError::CorruptState);
                }
                selected.insert(task.id.clone(), task.clone());
            }
        }
        Ok(Self {
            requested,
            tasks: selected,
        })
    }
    pub fn selected_len(&self) -> usize {
        self.tasks.len()
    }
}
impl TaskEvidenceReader for TaskEvidenceSnapshot {
    fn find(&self, id: &str) -> TaskResult<Option<&Task>> {
        if !self.requested.contains_key(id) {
            return Err(TaskError::Unavailable);
        }
        Ok(self.tasks.get(id))
    }
}
impl std::fmt::Debug for TaskEvidenceSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskEvidenceSnapshot")
            .field("requested", &self.requested.len())
            .field("selected", &self.tasks.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn task(id: &str) -> Task {
        Task {
            id: id.into(),
            owner: TaskOwner {
                session_id: "session".into(),
                user_id: "owner".into(),
            },
            source: format!("source:{id}"),
            created_at_ms: 1,
            status: TaskStatus::Queued,
            spec: TaskSpec {
                title: "任务".into(),
                instructions: "private body".repeat(600),
                max_steps: DEFAULT_STEPS,
                context: None,
            },
            cancel_requested: false,
            started_at_ms: None,
            finished_at_ms: None,
            usage: None,
            result: None,
            failure: None,
            notice: None,
            execution: None,
        }
    }
    fn key(task: &Task) -> TaskEvidenceKey {
        TaskEvidenceKey {
            id: task.id.clone(),
            owner: task.owner.clone(),
        }
    }
    #[test]
    fn selected_view_preserves_owned_body_and_distinguishes_unrequested_from_evicted() {
        let mut tasks: Vec<_> = (0..MAX_TASKS).map(|i| task(&format!("task-{i}"))).collect();
        let selected = tasks[1].clone();
        let missing = task("evicted");
        let view =
            TaskEvidenceSnapshot::select(&tasks, &[key(&selected), key(&selected), key(&missing)])
                .unwrap();
        assert_eq!(view.selected_len(), 1);
        assert_eq!(view.find(&selected.id).unwrap(), Some(&selected));
        assert!(view.find("evicted").unwrap().is_none());
        assert_eq!(view.find(&tasks[0].id), Err(TaskError::Unavailable));
        tasks[1].spec.instructions = "changed later".into();
        assert_eq!(view.find(&selected.id).unwrap(), Some(&selected));
        assert!(!format!("{view:?}").contains("private body"));
        let empty = TaskEvidenceSnapshot::select(&tasks, &[]).unwrap();
        assert_eq!(empty.selected_len(), 0);
        assert!(empty.find(&selected.id).is_err());
    }
    #[test]
    fn later_corruption_and_owner_substitution_reject_entire_view() {
        let original = task("a");
        let mut changed = original.clone();
        changed.owner.session_id = "elsewhere".into();
        assert!(matches!(
            TaskEvidenceSnapshot::select(&[changed.clone()], &[key(&original)]),
            Err(TaskError::CorruptState)
        ));
        assert!(matches!(
            TaskEvidenceSnapshot::select(
                std::slice::from_ref(&original),
                &[key(&original), key(&changed)]
            ),
            Err(TaskError::CorruptState)
        ));
        let duplicate = task("unselected");
        assert!(matches!(
            TaskEvidenceSnapshot::select(
                &[original.clone(), duplicate.clone(), duplicate],
                &[key(&original)]
            ),
            Err(TaskError::CorruptState)
        ));
        changed.id.clear();
        assert!(
            TaskEvidenceSnapshot::select(&[original.clone(), changed], &[key(&original)]).is_err()
        );
    }
    #[test]
    fn evidence_keys_validate_limits_and_keep_full_identity() {
        let original = task("a");
        let mut invalid = key(&original);
        invalid.owner.user_id.clear();
        assert!(TaskEvidenceSnapshot::select(&[], &[invalid]).is_err());
        assert!(matches!(
            TaskEvidenceSnapshot::select(&[], &vec![key(&original); MAX_TASK_STRATEGY_RECORDS + 1]),
            Err(TaskError::LimitReached)
        ));
        let legacy = TaskSnapshot {
            tasks: vec![original.clone()],
        };
        assert_eq!(
            TaskEvidenceReader::find(&legacy, "a").unwrap(),
            Some(&original)
        );
        assert!(
            TaskEvidenceReader::find(&legacy, "missing")
                .unwrap()
                .is_none()
        );
    }
}
