//! 有界文件监听。文件 I/O 只在独立监听线程执行，模型轮次只读取已验证缓存。
use super::{FileAgentPrompt, configuration};
use eve_llm_api::{LlmError, SystemPromptSnapshot, SystemPromptSource};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

const POLL: Duration = Duration::from_millis(250);
struct Shared {
    current: Mutex<Result<SystemPromptSnapshot, LlmError>>,
    stop: Mutex<bool>,
    changed: Condvar,
    alive: AtomicBool,
}
/// 同一路径的原子替换和同大小改写也会重新读取；不依赖文件时间戳判断内容。
/// 两次相同采样才发布，避免把编辑器短暂的空文件或未完成写入立刻投入使用。
/// 载入错误阻止新轮次，既有轮次持有自己的不可变快照；修复文件后自动恢复。
pub struct WatchedAgentPrompt {
    shared: Arc<Shared>,
}
impl WatchedAgentPrompt {
    pub fn new(source: FileAgentPrompt) -> Result<Self, LlmError> {
        Self::start(source, POLL)
    }
    fn start(source: FileAgentPrompt, period: Duration) -> Result<Self, LlmError> {
        let initial = source.load()?;
        let shared = Arc::new(Shared {
            current: Mutex::new(Ok(initial.clone())),
            stop: Mutex::new(false),
            changed: Condvar::new(),
            alive: AtomicBool::new(true),
        });
        let state = shared.clone();
        thread::Builder::new()
            .name("eve-agent-watch".into())
            .spawn(move || {
                struct Alive(Arc<Shared>);
                impl Drop for Alive {
                    fn drop(&mut self) {
                        self.0.alive.store(false, Ordering::Release);
                    }
                }
                let _alive = Alive(state.clone());
                let mut previous = Ok(initial);
                loop {
                    let Ok(stop) = state.stop.lock() else {
                        break;
                    };
                    let Ok((stop, _)) = state
                        .changed
                        .wait_timeout_while(stop, period, |stop| !*stop)
                    else {
                        break;
                    };
                    if *stop {
                        break;
                    }
                    drop(stop);
                    let next = source.load();
                    if next == previous {
                        let Ok(mut current) = state.current.lock() else {
                            break;
                        };
                        if *current != next {
                            *current = next.clone();
                        }
                    }
                    previous = next;
                }
            })
            .map_err(|_| configuration("AGENT 文件监听器无法启动"))?;
        Ok(Self { shared })
    }
}
impl SystemPromptSource for WatchedAgentPrompt {
    fn load(&self) -> Result<SystemPromptSnapshot, LlmError> {
        if !self.shared.alive.load(Ordering::Acquire) {
            return Err(configuration("AGENT 文件监听器已停止"));
        }
        self.shared
            .current
            .lock()
            .map_err(|_| configuration("AGENT 文件监听缓存不可用"))?
            .clone()
    }
}
impl Drop for WatchedAgentPrompt {
    fn drop(&mut self) {
        if let Ok(mut stop) = self.shared.stop.lock() {
            *stop = true;
            self.shared.changed.notify_one();
        }
        // 不在析构中等待可能阻塞的文件系统 I/O；线程下一安全点退出并释放共享状态。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(std::time::Instant::now() < deadline, "监听器未按期收敛");
            thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn same_size_replacement_invalid_input_and_recovery_keep_old_snapshots_immutable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("AGENT.md");
        fs::write(&path, "FIRST_RULE").unwrap();
        let source = WatchedAgentPrompt::start(
            FileAgentPrompt::new(&path).unwrap(),
            Duration::from_millis(10),
        )
        .unwrap();
        let old = source.load().unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "OTHER_RULE").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        until(|| {
            source
                .load()
                .is_ok_and(|snapshot| snapshot.text() == "OTHER_RULE")
        });
        assert_eq!(old.text(), "FIRST_RULE");
        assert_ne!(
            old.metadata().revision,
            source.load().unwrap().metadata().revision
        );
        fs::write(&path, [0xff]).unwrap();
        until(|| source.load().is_err());
        assert_eq!(fs::read(&path).unwrap(), [0xff]);
        fs::remove_file(&path).unwrap();
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, "RESTORED_RULE").unwrap();
        fs::rename(&replacement, &path).unwrap();
        until(|| {
            source
                .load()
                .is_ok_and(|snapshot| snapshot.text() == "RESTORED_RULE")
        });
        let weak = Arc::downgrade(&source.shared);
        drop(source);
        until(|| weak.upgrade().is_none());
    }
    #[test]
    fn empty_or_missing_source_fails_before_a_watcher_is_created() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("AGENT.md");
        assert!(WatchedAgentPrompt::new(FileAgentPrompt::new(&path).unwrap()).is_err());
        fs::write(&path, "").unwrap();
        assert!(WatchedAgentPrompt::new(FileAgentPrompt::new(&path).unwrap()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"");
    }
}
