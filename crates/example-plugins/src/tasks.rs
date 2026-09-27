//! 任务演示只依赖插件定义层，定时和执行由宿主后端负责。
use eve_plugin_api::{
    Cleanup, Plugin, PluginContext, PluginError, PluginFuture, PluginManifest, PluginResult,
    TaskMode, TaskSchedule, TaskSpec, TaskState,
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
            let state = ctx.clone();
            ctx.spawn_task(TaskSpec::new(
                "后台周期任务",
                TaskMode::Background,
                TaskSchedule::Every {
                    interval: Duration::from_secs(1),
                    runs: None,
                },
                Arc::new(move |_| {
                    let state = state.clone();
                    Box::pin(async move { state.state_set("background_started", b"yes".to_vec()) })
                }),
            )?)?;
            Ok(None)
        })
    }
}
