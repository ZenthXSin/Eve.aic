//! 真实 Mindustry 无头服务端验收：需要显式提供 EVE_MINDUSTRY_SERVER_JAR（以及 PATH 上的 java）。
//! 默认忽略，CI 的独立作业下载固定版本并校验摘要后运行。
use eve_practice_api::{ArtifactFile, PracticeDraft, PracticeRunner, Probe, RunExit};
use eve_practice_mindustry::{MindustryServerRunner, RuntimeCommand};
use std::path::PathBuf;

fn runner() -> MindustryServerRunner {
    let jar = std::env::var_os("EVE_MINDUSTRY_SERVER_JAR").expect("需要 EVE_MINDUSTRY_SERVER_JAR");
    MindustryServerRunner::new(RuntimeCommand::default(), PathBuf::from(jar)).unwrap()
}

fn draft(block: &str, probes: &[(&str, &str)]) -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![
            ArtifactFile {
                path: "mod.hjson".into(),
                content: "name: \"eve-sample\"\ndisplayName: \"Eve Sample\"\nauthor: \"Eve\"\nversion: \"1.0\"\nminGameVersion: 146\n".into(),
            },
            ArtifactFile {
                path: "content/blocks/sample-wall.hjson".into(),
                content: block.into(),
            },
        ],
        probes: probes
            .iter()
            .map(|(property, expected)| Probe {
                subject: "eve-sample-sample-wall".into(),
                property: (*property).into(),
                expected: (*expected).into(),
            })
            .collect(),
        rationale: "最小方块".into(),
        notes_used: vec![],
    }
}

#[tokio::test]
#[ignore = "需要显式提供真实 Mindustry 服务端 jar"]
async fn real_server_verifies_a_data_only_mod_and_rejects_silent_fallbacks() {
    let runner = runner();
    assert!(
        runner
            .profile()
            .runtime
            .starts_with("Mindustry server jar sha256:")
    );

    let good = draft(
        "type: Wall\nhealth: 520\nsize: 1\nrequirements: [ copper/6 ]\ncategory: defense\n",
        &[
            ("exists", "true"),
            ("class", "Wall"),
            ("health", "520"),
            ("size", "1"),
            ("category", "defense"),
        ],
    );
    assert!(runner.check(&good).is_empty(), "{:?}", runner.check(&good));
    let workspace = tempfile::tempdir().unwrap();
    let evidence = runner.run(&good, workspace.path()).await;
    evidence.validate(&good).unwrap();
    println!("{}", evidence.log_excerpt);
    assert_eq!(
        evidence.exit,
        RunExit::Completed,
        "{}",
        evidence.log_excerpt
    );
    assert!(
        evidence.runtime_version.contains("build"),
        "{}",
        evidence.runtime_version
    );
    assert!(
        evidence.loaded && evidence.warnings.is_empty(),
        "{}",
        evidence.log_excerpt
    );
    assert!(evidence.verified(&good), "{}", evidence.log_excerpt);
    assert!(
        !evidence
            .log_excerpt
            .contains(&*workspace.path().to_string_lossy())
    );

    // 类型写错：游戏只给警告并退回普通方块，仍显示已加载；必须因警告与探测不符而未验证。
    let fallback = draft(
        "type: NotARealBlockType\nhealth: 520\nrequirements: [ copper/6 ]\n",
        &[("class", "Wall"), ("health", "520")],
    );
    assert!(runner.check(&fallback).is_empty());
    let workspace = tempfile::tempdir().unwrap();
    let evidence = runner.run(&fallback, workspace.path()).await;
    evidence.validate(&fallback).unwrap();
    println!("{}", evidence.log_excerpt);
    assert_eq!(evidence.exit, RunExit::Completed);
    assert!(evidence.loaded, "游戏仍报告已加载");
    assert!(
        evidence
            .warnings
            .iter()
            .any(|warning| warning.contains("NotARealBlockType"))
    );
    assert_eq!(evidence.probes[0].actual.as_deref(), Some("Block"));
    assert!(!evidence.verified(&fallback));

    // 引用不存在的物品：游戏报告内容错误并退出，证据保留出错文件与原因。
    let broken = draft(
        "type: Wall\nhealth: 300\nrequirements: [ notanitem/6 ]\ncategory: defense\n",
        &[("exists", "true")],
    );
    let workspace = tempfile::tempdir().unwrap();
    let evidence = runner.run(&broken, workspace.path()).await;
    evidence.validate(&broken).unwrap();
    println!("{}", evidence.log_excerpt);
    assert_ne!(evidence.exit, RunExit::Completed);
    assert!(
        evidence
            .warnings
            .iter()
            .any(|warning| warning.contains("notanitem")),
        "{:?}",
        evidence.warnings
    );
    assert!(!evidence.verified(&broken));
}
