//! 用真实解释器运行继承来的 JSON 校验规则：证明 hjson 展开、类型分派与问题翻译都对得上。
//! 这些测试需要 python3；没有解释器的环境会被跳过并说明原因，不伪装成通过。
use eve_practice_api::ArtifactFile;
use eve_practice_mindustry::{JsonPrecheck, MindustryJsonRules, PythonCommand};
use std::path::PathBuf;

fn precheck() -> Option<JsonPrecheck> {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/eve-portable-mindustry-json-validator");
    Some(JsonPrecheck::new(
        MindustryJsonRules::open(&directory).expect("规则副本应与固定版本一致"),
        PythonCommand::default(),
    ))
}

fn available() -> bool {
    let python = if cfg!(windows) { "python" } else { "python3" };
    std::process::Command::new(python)
        .arg("--version")
        .output()
        .is_ok()
}

fn file(path: &str, content: &str) -> ArtifactFile {
    ArtifactFile {
        path: path.into(),
        content: content.into(),
    }
}

#[tokio::test]
async fn hjson_content_is_expanded_and_reported_with_game_style_messages() {
    let Some(check) = precheck() else { return };
    if !available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    let files = vec![
        file(
            "content/blocks/sample-wall.hjson",
            "type: Wall\nhealth: 520\n# 注释\nrequirements: [\n  copper/6\n  graphite/2,\n]\n",
        ),
        file(
            "content/units/bad-unit.hjson",
            "type: flying\nweapons: [\n  { reload: 10, bullet: { type: no-such-bullet } }\n]\n",
        ),
        file("content/liquids/number-color.hjson", "color: 0.5\n"),
    ];
    let issues = check.check("eve-sample", &files).await.unwrap();
    let joined = issues.join("\n");
    assert!(
        joined.contains("Bullet type 'no-such-bullet' not found in ClassMap"),
        "{joined}"
    );
    assert!(
        joined.contains("Colors should strings, not numbers"),
        "{joined}"
    );
    // 每个问题都标出文件，草稿器才能定位到该改哪一份。
    assert!(
        joined.contains("content/units/bad-unit.hjson")
            && joined.contains("content/liquids/number-color.hjson"),
        "{joined}"
    );
    // 完全合规的方块只留一条“缺 name”的提示，并且按警告而不是错误报告。
    let wall_only = issues
        .iter()
        .filter(|issue| issue.contains("sample-wall.hjson"))
        .collect::<Vec<_>>();
    assert!(
        wall_only
            .iter()
            .all(|issue| issue.starts_with("JSON 预检警告")),
        "{wall_only:?}"
    );
}

#[tokio::test]
async fn invalid_json_is_reported_as_an_error_instead_of_passing_silently() {
    let Some(check) = precheck() else { return };
    if !available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    let files = vec![file(
        "content/blocks/broken.hjson",
        "type: Wall\nhealth: [\n",
    )];
    let issues = check.check("eve-sample", &files).await.unwrap();
    assert!(
        issues
            .iter()
            .any(|issue| issue.starts_with("JSON 预检错误")),
        "{issues:?}"
    );
}

#[tokio::test]
async fn mod_manifest_is_checked_without_being_treated_as_game_content() {
    let Some(check) = precheck() else { return };
    if !available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    // 清单不是 content 目录下的文件：只做 JSON/hjson 解析检查，不应按某种内容类型报错。
    let files = vec![file(
        "mod.hjson",
        "name: \"eve-sample\"\ndisplayName: \"Eve Sample\"\nversion: \"1.0\"\nminGameVersion: 146\n",
    )];
    let issues = check.check("eve-sample", &files).await.unwrap();
    assert!(
        issues
            .iter()
            .all(|issue| !issue.contains("JSON parse error")),
        "{issues:?}"
    );
    // 连键值分隔冒号都没有的清单不是 hjson，必须报出来而不是当作通过。
    let not_json = vec![file("mod.hjson", "{ \"name\" \"eve-sample\" }")];
    let issues = check.check("eve-sample", &not_json).await.unwrap();
    assert!(
        issues
            .iter()
            .any(|issue| issue.contains("既不是 JSON，也不是可展开的 hjson")),
        "{issues:?}"
    );
}

#[tokio::test]
async fn scripts_and_sprites_are_never_sent_to_the_checker() {
    let Some(check) = precheck() else { return };
    if !available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    // 这些路径本来就会被结构检查拒绝；预检再多看一眼没有意义，也不该把脚本内容送进解释器。
    let files = vec![
        file("scripts/main.js", "throw new Error('#')"),
        file("sprites/block.png", "==== end ===="),
    ];
    let issues = check.check("eve-sample", &files).await.unwrap();
    assert!(issues.is_empty(), "{issues:?}");
}

#[tokio::test]
async fn startup_probe_runs_the_real_rules_and_reports_a_missing_interpreter() {
    let Some(check) = precheck() else { return };
    if !available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    check.probe().await.expect("真实规则应能检查最小清单");
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/eve-portable-mindustry-json-validator");
    let missing = JsonPrecheck::new(
        MindustryJsonRules::open(&directory).unwrap(),
        PythonCommand {
            program: "eve-missing-python-for-test".into(),
            prefix_args: vec![],
        },
    );
    assert!(missing.probe().await.unwrap_err().contains("无法启动"));
}

fn practice_run() -> eve_practice_api::PracticeRun {
    use eve_practice_api::{PracticeRun, PracticeStatus, PracticeTask, RunnerProfile};
    PracticeRun {
        id: "practice-run".into(),
        task: PracticeTask {
            goal_id: "goal-1".into(),
            goal_revision: 1,
            owner: "user-a".into(),
            brief: "做一面墙".into(),
            brief_truncated: false,
            notes: vec![],
        },
        runner: RunnerProfile {
            runner_id: "mindustry-content-mod:v1".into(),
            runtime: "test".into(),
            domain: "Mindustry".into(),
            layout: "mod.hjson".into(),
            properties: vec![],
        },
        drafter_version: "d:v1".into(),
        started_at_ms: 1,
        finished_at_ms: None,
        status: PracticeStatus::Running,
        attempts: vec![],
    }
}

fn wall_draft(size: u32) -> eve_practice_api::PracticeDraft {
    eve_practice_api::PracticeDraft {
        applicable: true,
        files: vec![
            file(
                "mod.hjson",
                "name: \"eve-sample\"\nversion: \"1.0\"\nminGameVersion: 146\n",
            ),
            // 没写 name：规则会给出“按文件名推断”的警告，游戏照常加载。
            file(
                "content/blocks/sample-wall.hjson",
                &format!(
                    "type: Wall\nhealth: 520\nsize: {size}\nrequirements: [ copper/6 ]\ncategory: defense\n"
                ),
            ),
        ],
        probes: vec![],
        rationale: String::new(),
        notes_used: vec![],
    }
}

#[test]
fn only_errors_block_the_run_and_warnings_reach_the_real_game() {
    use eve_practice_api::DraftCheck;
    use std::sync::Arc;
    let Some(precheck) = precheck() else { return };
    if !available() {
        eprintln!("跳过：本机没有 python3，无法运行继承来的 JSON 校验规则");
        return;
    }
    let check = eve_practice_mindustry::JsonDraftCheck::new(
        precheck,
        Arc::new(|reason: &str| panic!("真实规则不应不可用：{reason}")),
    );
    assert!(
        check
            .check(&practice_run(), &wall_draft(1), 1)
            .unwrap()
            .is_empty()
    );
    let issues = check.check(&practice_run(), &wall_draft(9), 1).unwrap();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(issues[0].starts_with("JSON 预检错误") && issues[0].contains("larger than 6"));
}

#[test]
fn unavailable_interpreter_during_practice_adds_no_issue_and_is_reported() {
    use eve_practice_api::DraftCheck;
    use std::sync::{Arc, Mutex};
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/eve-portable-mindustry-json-validator");
    let reasons = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = reasons.clone();
    let check = eve_practice_mindustry::JsonDraftCheck::new(
        JsonPrecheck::new(
            MindustryJsonRules::open(&directory).unwrap(),
            PythonCommand {
                program: "eve-missing-python-for-test".into(),
                prefix_args: vec![],
            },
        ),
        Arc::new(move |reason: &str| seen.lock().unwrap().push(reason.to_string())),
    );
    // 解释器不可用时不能报“没有问题”给用户，但也不让实践失败：照常实际运行，原因交给宿主记录。
    assert!(
        check
            .check(&practice_run(), &wall_draft(9), 1)
            .unwrap()
            .is_empty()
    );
    let reasons = reasons.lock().unwrap();
    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("无法启动"), "{reasons:?}");
}
