//! 继承的先验规则与“从实践里自己锻造出规则”走同一条运行前检查路径：
//! 两者给出的问题都带 `[预检] ` 前缀、都只写进本次尝试的记录，都不改变真实运行与否。
//! 这里用真实内核、真实账本和真实 Python 规则，证明接线顺序与记账方式都成立。
mod support;

use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_practice_api::*;
use eve_practice_mindustry::{JsonPrecheck, MindustryJsonRules, PythonCommand};
use eve_practice_plugin::{PracticeController, PracticePlugin, Practitioner};
use eve_toolforge_plugin::{ForgedChecks, ToolForgeController, ToolForgePlugin};
use std::{path::Path, sync::Arc};

fn python_available() -> bool {
    std::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
        .arg("--version")
        .output()
        .is_ok()
}

fn profile() -> RunnerProfile {
    RunnerProfile {
        runner_id: "eve-mindustry-json-lint:v1".into(),
        runtime: format!(
            "eve-portable@e7e43b4 vendor.py sha256:{}",
            eve_practice_mindustry::rules::VENDOR_SHA256
        ),
        domain: "Mindustry 内容文件".into(),
        layout: "mod.hjson 与 content/<类型>/<名称>.hjson".into(),
        properties: vec!["exists".into()],
    }
}

fn mod_draft(content: &str) -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![
            ArtifactFile {
                path: "mod.hjson".into(),
                content:
                    "name: \"eve-sample\"\ndisplayName: \"Eve Sample\"\nversion: \"1.0\"\nminGameVersion: 146\n"
                        .into(),
            },
            ArtifactFile {
                path: "content/blocks/sample-heater.hjson".into(),
                content: content.into(),
            },
        ],
        probes: vec![Probe {
            subject: "eve-sample-sample-heater".into(),
            property: "exists".into(),
            expected: "true".into(),
        }],
        rationale: "最小方块".into(),
        notes_used: vec![],
    }
}

struct OneDraft {
    draft: PracticeDraft,
}
impl PracticeDrafter for OneDraft {
    fn version(&self) -> &str {
        "one-draft:v1"
    }
    fn draft(&self, _request: DraftRequest) -> PracticeFuture<'_, PracticeDraft> {
        Box::pin(async move { Ok(self.draft.clone()) })
    }
}

async fn open() -> (Kernel, PracticeController, ToolForgeController) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(FileStateStore::open(directory.path()).unwrap());
    // 目录必须存活到账本用完；把 TempDir 交给内核所在的作用域持有。
    std::mem::forget(directory);
    let kernel = Kernel::with_services(KernelServices {
        state: store,
        ..KernelServices::default()
    });
    let practice = PracticePlugin::new().unwrap();
    let tools = ToolForgePlugin::new().unwrap();
    let (practice_admin, tool_admin) = (practice.controller(), tools.controller());
    kernel.register(Box::new(practice)).unwrap();
    kernel.register(Box::new(tools)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, practice_admin, tool_admin)
}

async fn practice_once(
    practice: &PracticeController,
    draft: PracticeDraft,
    check: Arc<dyn DraftCheck>,
    workspace: &Path,
) -> PracticeRun {
    let clock = std::sync::atomic::AtomicU64::new(1_000);
    let next = || clock.fetch_add(10, std::sync::atomic::Ordering::SeqCst);
    let run = practice
        .begin(
            PracticeTask {
                goal_id: "goal-1".into(),
                goal_revision: 1,
                owner: "user-a".into(),
                brief: "练习做一个会发热的方块".into(),
                brief_truncated: false,
                notes: vec![],
            },
            &profile(),
            "one-draft:v1",
            1_000,
        )
        .unwrap()
        .unwrap();
    let practitioner = Practitioner::new(
        Arc::new(practice.clone()),
        Arc::new(OneDraft { draft }),
        Arc::new(support::NoRun::new(profile())),
    )
    .with_check(check);
    practitioner.practice(&run, workspace, next).await.unwrap()
}

#[tokio::test]
async fn inherited_rules_run_before_the_real_run_and_are_recorded_as_precheck_issues() {
    if !python_available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    let directory = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/eve-portable-mindustry-json-validator");
    let rules = MindustryJsonRules::open(&directory).unwrap();
    let check: Arc<dyn DraftCheck> = Arc::new(support::JsonPrecheckCheck::new(JsonPrecheck::new(
        rules,
        PythonCommand::default(),
    )));
    let workspace = tempfile::tempdir().unwrap();
    let (kernel, practice, tools) = open().await;
    let run = practice_once(
        &practice,
        mod_draft("type: Wall\nhealth: 520\nsize: 9\n"),
        check,
        workspace.path(),
    )
    .await;
    let attempt = run
        .attempts
        .first()
        .unwrap_or_else(|| panic!("实践没有留下尝试：{run:?}"));
    let joined = attempt.issues.join("\n");
    assert!(
        attempt
            .issues
            .iter()
            .all(|issue| issue.starts_with(DRAFT_CHECK_MARK)),
        "{joined}"
    );
    assert!(
        joined.contains("Blocks cannot be larger than 6"),
        "继承来的规则应当指出尺寸超限：{joined}"
    );
    // 预检问题不进能力缺口：锻造只针对真实运行暴露、反复出现的问题。
    let gaps = capability_gaps(&practice.snapshot().unwrap(), "user-a");
    assert!(
        gaps.iter()
            .all(|gap| !gap.key.contains("Blocks cannot be larger than 6")),
        "{gaps:?}"
    );

    // 锻造检查在同一份草稿上照常可用；两者可以同时在场，互不改写对方的结论。
    let forged = ForgedChecks::new(Arc::new(tools.clone()));
    let findings = forged
        .check(&run, attempt.draft.as_ref().unwrap(), 2_000)
        .unwrap();
    assert!(findings.is_empty(), "{findings:?}");
    kernel.stop_all().await.unwrap();
}
