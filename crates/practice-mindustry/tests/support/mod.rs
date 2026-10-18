//! 预检接线的测试支撑：不实际运行的运行器。
//! 只在测试里使用，不进入发布产物。
#![allow(dead_code)]

use eve_practice_api::*;
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
