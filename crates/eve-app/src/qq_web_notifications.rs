//! QQ 共宿主 Web 通知收集器：不依赖浏览器，保存或结算失败进入宿主统一收尾。
use crate::{AppError, web_notifications::Inbox};
use std::sync::Arc;
use tokio::{sync::watch, task::JoinHandle};

pub(crate) struct Background {
    stop: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: JoinHandle<Result<(), AppError>>,
}

impl Background {
    pub(crate) fn start(
        inbox: Arc<Inbox>,
        tasks: Option<Arc<dyn eve_task_api::TaskAdmin>>,
        triggers: Option<Arc<dyn eve_trigger_api::TriggerAdmin>>,
    ) -> Self {
        let (stop, mut stopped) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        let task = tokio::spawn(async move {
            let result = async {
                let mut capacity_reported = false;
                loop {
                    tokio::select! {
                        biased;
                        changed = stopped.changed() => {
                            if changed.is_err() || *stopped.borrow_and_update() {
                                return Ok(());
                            }
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                            match inbox.collect_available(
                                tasks.as_deref(), triggers.as_deref(), crate::qq_tasks::now_ms(),
                            ) {
                                Ok(()) => capacity_reported = false,
                                Err(eve_web_panel_api::PanelError::Capacity) => {
                                    if !capacity_reported {
                                        eprintln!("EVE_WEB_INBOX_CAPACITY 通知容量已满，待报告结果保留；可在面板读完通知后继续收集");
                                        capacity_reported = true;
                                    }
                                }
                                Err(_) => return Err("Web 通知保存或结算失败；保留原记录".into()),
                            }
                        }
                    }
                }
            }
            .await;
            let _ = done.send(true);
            result
        });
        Self {
            stop,
            finished,
            task,
        }
    }

    pub(crate) fn finished(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }

    pub(crate) fn request_stop(&self) {
        let _ = self.stop.send(true);
    }

    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        self.task
            .await
            .map_err(|_| "Web 通知收集后台异常；保留原记录")?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_task_api::{TaskAdmin, TaskOwner, TaskSpec};
    use eve_web_panel_api::WebInbox;

    #[tokio::test]
    async fn task_only_host_collects_without_browser_and_reports_storage_failure() {
        let root = tempfile::tempdir().unwrap();
        let kernel = eve_kernel::Kernel::new();
        let plugin = eve_task_plugin::TaskPlugin::new().unwrap();
        let tasks = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin)).unwrap();
        kernel.start_all().await.unwrap();
        let owner = TaskOwner {
            session_id: "web:chat:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            user_id: crate::web_chat::USER.into(),
        };
        let task = tasks
            .create(
                &owner,
                TaskSpec {
                    title: "已取消的后台任务".into(),
                    instructions: "等待宿主结果".into(),
                    max_steps: 1,
                    context: None,
                },
                "test-inbox-background",
                1,
            )
            .unwrap();
        tasks.cancel(&owner.user_id, &task.id, 2).unwrap();
        let inbox = Arc::new(Inbox::open(root.path()).unwrap());
        let background = Background::start(inbox.clone(), Some(tasks), None);
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                if !inbox.list(None, 10).unwrap().items.is_empty() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(inbox.list(None, 10).unwrap().items.len(), 1);
        kernel.stop_all().await.unwrap();
        let mut finished = background.finished();
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            while !*finished.borrow_and_update() {
                finished.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(background.stop().await.is_err());
    }
}
