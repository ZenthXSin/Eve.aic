//! 调度只复制轻量元数据，正文由已核验的完整身份单独读取。
use crate::*;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSchedulingEntry {
    pub id: String,
    pub owner: TaskOwner,
    pub source: String,
    pub created_at_ms: u64,
    pub status: TaskStatus,
    pub cancel_requested: bool,
    pub recoveries: u32,
    pub resume_at_ms: Option<u64>,
}
impl From<&Task> for TaskSchedulingEntry {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.clone(),
            owner: task.owner.clone(),
            source: task.source.clone(),
            created_at_ms: task.created_at_ms,
            status: task.status,
            cancel_requested: task.cancel_requested,
            recoveries: task
                .execution
                .as_ref()
                .map_or(0, |execution| execution.recoveries),
            resume_at_ms: task
                .execution
                .as_ref()
                .and_then(|execution| execution.resume_at_ms),
        }
    }
}
impl TaskSchedulingEntry {
    /// 完整读取不能把目录中的任务换成另一身份、另一来源或另一创建记录。
    pub fn matches(&self, task: &Task) -> bool {
        self.id == task.id
            && self.owner == task.owner
            && self.source == task.source
            && self.created_at_ms == task.created_at_ms
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSchedulingSnapshot {
    pub tasks: Vec<TaskSchedulingEntry>,
}
impl TaskSchedulingSnapshot {
    pub fn validate(&self) -> TaskResult<()> {
        if self.tasks.len() > MAX_TASKS {
            return Err(TaskError::CorruptState);
        }
        let mut ids = BTreeSet::new();
        for task in &self.tasks {
            validate_id(&task.id).map_err(|_| TaskError::CorruptState)?;
            validate_owner(&task.owner).map_err(|_| TaskError::CorruptState)?;
            validate_id(&task.source).map_err(|_| TaskError::CorruptState)?;
            if !ids.insert(&task.id)
                || task.created_at_ms == 0
                || task.recoveries > MAX_AUTO_RECOVERIES
                || task.resume_at_ms == Some(0)
            {
                return Err(TaskError::CorruptState);
            }
        }
        Ok(())
    }
    pub fn queued(&self) -> Vec<&TaskSchedulingEntry> {
        let mut tasks: Vec<_> = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Queued)
            .collect();
        tasks.sort_by_key(|task| (task.created_at_ms, &task.id));
        tasks
    }
    pub fn running(&self) -> impl Iterator<Item = &TaskSchedulingEntry> {
        self.tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn task(id: &str, at: u64) -> Task {
        Task {
            id: id.into(),
            owner: TaskOwner {
                session_id: "session".into(),
                user_id: "user".into(),
            },
            source: format!("source:{id}"),
            created_at_ms: at,
            status: TaskStatus::Queued,
            spec: TaskSpec {
                title: "任务".into(),
                instructions: "不进入目录的说明".into(),
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
    #[test]
    fn complete_directory_preserves_queue_order_and_running_recovery_metadata() {
        let mut running = task("running", 1);
        running.status = TaskStatus::Running;
        running.cancel_requested = true;
        running.execution = Some(TaskExecution {
            recoveries: 2,
            resume_at_ms: Some(100),
            ..Default::default()
        });
        let tasks = [task("c", 20), running, task("b", 10), task("a", 10)];
        let directory = TaskSchedulingSnapshot {
            tasks: tasks.iter().map(TaskSchedulingEntry::from).collect(),
        };
        directory.validate().unwrap();
        assert_eq!(
            directory
                .queued()
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        let [running] = directory.running().collect::<Vec<_>>()[..] else {
            panic!("expected one running entry")
        };
        assert!(running.cancel_requested);
        assert_eq!((running.recoveries, running.resume_at_ms), (2, Some(100)));
    }
    #[test]
    fn substituted_task_identity_source_and_creation_are_rejected() {
        let original = task("a", 1);
        let entry = TaskSchedulingEntry::from(&original);
        assert!(entry.matches(&original));
        for variant in 0..5 {
            let mut changed = original.clone();
            match variant {
                0 => changed.id = "other".into(),
                1 => changed.owner.session_id = "other".into(),
                2 => changed.owner.user_id = "other".into(),
                3 => changed.source = "other".into(),
                _ => changed.created_at_ms += 1,
            }
            assert!(!entry.matches(&changed));
        }
    }
    #[test]
    fn duplicate_or_invalid_directory_is_rejected_before_selection() {
        let entry = TaskSchedulingEntry::from(&task("a", 1));
        assert_eq!(
            TaskSchedulingSnapshot {
                tasks: vec![entry.clone(), entry.clone()]
            }
            .validate(),
            Err(TaskError::CorruptState)
        );
        for variant in 0..4 {
            let mut changed = entry.clone();
            changed.id = "another".into();
            match variant {
                0 => changed.owner.user_id.clear(),
                1 => changed.created_at_ms = 0,
                2 => changed.recoveries = MAX_AUTO_RECOVERIES + 1,
                _ => changed.resume_at_ms = Some(0),
            }
            assert_eq!(
                TaskSchedulingSnapshot {
                    tasks: vec![entry.clone(), changed]
                }
                .validate(),
                Err(TaskError::CorruptState)
            );
        }
    }
}
