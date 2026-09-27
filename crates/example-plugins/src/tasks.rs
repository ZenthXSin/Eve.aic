//! 任务演示只依赖插件定义层，定时和执行由宿主后端负责。
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult, Task,
    TaskMode, TaskSchedule, TaskScheduler, TaskSignal, TaskSpec, TaskState, TaskTypeId,
};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const TASK_DEMO: &str = "demo.tasks";

pub struct TaskDemoPlugin {
    manifest: PluginManifest,
}
impl TaskDemoPlugin {
    pub fn new() -> PluginResult<Self> {
        Ok(Self {
            manifest: PluginManifest::new(TASK_DEMO, "0.1.0")?,
        })
    }
}
impl Plugin for TaskDemoPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn start(&mut self, ctx: PluginContext) -> PluginFuture<'_, Option<Cleanup>> {
        Box::pin(async move {
            let schedules = [
                TaskSchedule::Immediate,
                TaskSchedule::After(Duration::from_millis(1)),
                TaskSchedule::At(SystemTime::now() + Duration::from_millis(10)),
                TaskSchedule::Every {
                    interval: Duration::from_millis(1),
                    runs: Some(3),
                },
            ];
            let mut completed = 0_u32;
            for schedule in schedules {
                let report = ctx
                    .run_foreground(TaskSpec::new(
                        "前台任务演示",
                        TaskMode::Foreground,
                        schedule,
                        Arc::new(|_| Box::pin(async { Ok(()) })),
                    )?)
                    .await?;
                if report.state != TaskState::Finished || !report.errors.is_empty() {
                    return Err(PluginError::Task(format!("前台演示失败：{report:?}")));
                }
                completed += report.runs;
            }
            ctx.state_set("foreground_runs", completed.to_string().into_bytes())?;
            // 新任务类型与新调度策略均在插件侧定义，内核无需认识这些结构体。
            let task_type = TaskTypeId::new("demo.write-state")?;
            let report = ctx
                .run_foreground(TaskSpec::from_task(
                    "自定义状态写入任务",
                    TaskMode::Foreground,
                    TaskSchedule::Custom {
                        name: "demo.increasing-delay".into(),
                        max_runs: Some(3),
                        factory: Arc::new(|| Box::new(IncreasingDelay { next_ms: 1 })),
                    },
                    Arc::new(WriteStateTask {
                        ctx: ctx.clone(),
                        task_type: task_type.clone(),
                        key: "custom_task_value",
                        value: b"custom task executed".to_vec(),
                    }),
                )?)
                .await?;
            if report.state != TaskState::Finished || report.task_type != task_type {
                return Err(PluginError::Task(format!("自定义任务演示失败：{report:?}")));
            }
            ctx.state_set("custom_task_runs", report.runs.to_string().into_bytes())?;
            ctx.spawn_task(TaskSpec::from_task(
                "后台周期任务",
                TaskMode::Background,
                TaskSchedule::Every {
                    interval: Duration::from_secs(1),
                    runs: None,
                },
                Arc::new(WriteStateTask {
                    ctx: ctx.clone(),
                    task_type,
                    key: "background_started",
                    value: b"yes".to_vec(),
                }),
            )?)?;
            Ok(None)
        })
    }
}

/// 插件自己的强类型参数与行为；同一类型可用于前台或后台。
struct WriteStateTask {
    ctx: PluginContext,
    task_type: TaskTypeId,
    key: &'static str,
    value: Vec<u8>,
}

impl Task for WriteStateTask {
    fn task_type(&self) -> TaskTypeId {
        self.task_type.clone()
    }

    fn execute(&self, _signal: Arc<dyn TaskSignal>) -> PluginFuture<'_, ()> {
        Box::pin(async move { self.ctx.state_set(self.key, self.value.clone()) })
    }
}

/// 逐次增长的间隔，由默认执行器等待；插件不依赖 Tokio。
struct IncreasingDelay {
    next_ms: u64,
}

impl TaskScheduler for IncreasingDelay {
    fn next(&mut self, _completed: u32) -> PluginFuture<'_, Option<Duration>> {
        Box::pin(async move {
            let delay = Duration::from_millis(self.next_ms);
            self.next_ms += 1;
            Ok(Some(delay))
        })
    }
}
