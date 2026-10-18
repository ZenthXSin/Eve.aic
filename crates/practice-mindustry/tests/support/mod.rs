//! 预检接线的测试支撑：不实际运行的运行器、以及把继承来的规则适配成 `DraftCheck` 的薄封装。
//! 只在测试里使用，不进入发布产物。
#![allow(dead_code)]

use eve_practice_api::*;
use eve_practice_mindustry::JsonPrecheck;
use std::path::Path;

pub struct NoRun {
    profile: RunnerProfile,
}

impl NoRun {
    pub fn new(profile: RunnerProfile) -> Self {
        Self { profile }
    }
}

impl PracticeRunner for NoRun {
    fn profile(&self) -> &RunnerProfile {
        &self.profile
    }
    fn check(&self, _draft: &PracticeDraft) -> Vec<String> {
        vec![]
    }
    fn run<'a>(
        &'a self,
        _draft: &'a PracticeDraft,
        _workspace: &'a Path,
    ) -> BoxFuture<'a, RunEvidence> {
        Box::pin(async {
            RunEvidence {
                runtime_version: String::new(),
                exit: RunExit::StartFailed,
                loaded: false,
                warnings: vec![],
                probes: vec![],
                log_excerpt: "测试不实际运行".into(),
                log_sha256: "0".repeat(64),
                log_bytes: 0,
                duration_ms: 0,
            }
        })
    }
}

/// 从真实规则到运行前检查的适配：取 mod 清单的 name 作为命名前缀，送检清单与内容文件。
pub struct JsonPrecheckCheck {
    precheck: JsonPrecheck,
}

impl JsonPrecheckCheck {
    pub fn new(precheck: JsonPrecheck) -> Self {
        Self { precheck }
    }
}

impl DraftCheck for JsonPrecheckCheck {
    fn check(
        &self,
        _run: &PracticeRun,
        draft: &PracticeDraft,
        _at_ms: u64,
    ) -> PracticeResult<Vec<String>> {
        let mod_name = draft
            .files
            .iter()
            .filter(|file| file.path == "mod.hjson" || file.path == "mod.json")
            .find_map(|file| {
                file.content.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == "name").then(|| value.trim().trim_matches('"').to_string())
                })
            })
            .unwrap_or_else(|| "mod".into());
        // 同步契约：这里阻塞等待真实规则跑完；单次调用有 60 秒上限。
        // 测试与宿主可能已经在多线程运行时里，所以另起一个线程跑自己的运行时。
        let (sender, receiver) = std::sync::mpsc::channel();
        {
            let precheck = self.precheck.clone();
            let files = draft.files.clone();
            std::thread::spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())
                    .and_then(|runtime| {
                        runtime
                            .block_on(precheck.check(&mod_name, &files))
                            .map_err(|error| error.to_string())
                    });
                let _ = sender.send(result);
            });
        }
        receiver
            .recv()
            .map_err(|_| PracticeError::Unavailable)?
            .map_err(|error| {
                eprintln!("JSON 预检失败：{error}");
                PracticeError::Unavailable
            })
    }
}
