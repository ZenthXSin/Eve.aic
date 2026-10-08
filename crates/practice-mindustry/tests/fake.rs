//! 用确定性替身服务端验证运行器协议：工作目录、环境隔离、警告、崩溃与探测比较。
//! 替身只证明协议与证据处理；真实游戏行为由 `tests/real.rs` 在 CI 的独立作业中验证。
use eve_practice_api::{ArtifactFile, PracticeDraft, PracticeRunner, Probe, RunExit};
use eve_practice_mindustry::{MindustryServerRunner, RuntimeCommand};
use serde_json::Value;
use std::path::PathBuf;

fn python() -> &'static str {
    // Windows 运行器上的 python3 可能只是商店占位命令。
    if cfg!(windows) { "python" } else { "python3" }
}

fn runner(record: &std::path::Path) -> (MindustryServerRunner, tempfile::TempDir) {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../connectors/qqbot/test/fake_mindustry.py")
        .canonicalize()
        .unwrap();
    let jar = tempfile::tempdir().unwrap();
    let path = jar.path().join("server-release.jar");
    std::fs::write(&path, b"PK\x03\x04fake jar").unwrap();
    let runner = MindustryServerRunner::new(
        RuntimeCommand {
            program: python().into(),
            prefix_args: vec![script.into(), "--record".into(), record.into()],
        },
        path,
    )
    .unwrap();
    (runner, jar)
}

fn draft(block: &str, probes: &[(&str, &str)]) -> PracticeDraft {
    PracticeDraft {
        applicable: true,
        files: vec![
            ArtifactFile {
                path: "mod.hjson".into(),
                content: "name: \"eve-sample\"\ndisplayName: \"Eve Sample\"\nversion: \"1.0\"\nminGameVersion: 146\n".into(),
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
async fn runner_writes_only_into_the_workspace_and_turns_console_output_into_evidence() {
    let record_dir = tempfile::tempdir().unwrap();
    let record = record_dir.path().join("launches.jsonl");
    let (runner, _jar) = runner(&record);
    assert!(
        runner
            .profile()
            .runtime
            .starts_with("Mindustry server jar sha256:")
    );

    let good = draft(
        "type: Wall\nhealth: 520\nrequirements: [ copper/6 ]\ncategory: defense\n",
        &[
            ("exists", "true"),
            ("class", "Wall"),
            ("health", "520.0"),
            ("size", "1"),
        ],
    );
    assert!(runner.check(&good).is_empty());
    let workspace = tempfile::tempdir().unwrap();
    let evidence = runner.run(&good, workspace.path()).await;
    evidence.validate(&good).unwrap();
    assert_eq!(
        evidence.exit,
        RunExit::Completed,
        "{}",
        evidence.log_excerpt
    );
    assert_eq!(evidence.runtime_version, "Mindustry fake / build 160.7");
    assert!(evidence.verified(&good), "{}", evidence.log_excerpt);
    assert!(evidence.log_excerpt.contains("<workspace>/config/mods"));
    assert!(
        !evidence
            .log_excerpt
            .contains(&*workspace.path().to_string_lossy())
    );
    let written = workspace
        .path()
        .join("config/mods/eve-sample/content/blocks/sample-wall.hjson");
    assert!(written.exists(), "产物只写入工作目录下的 mods 目录");
    let launch: Value = serde_json::from_str(
        std::fs::read_to_string(&record)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        PathBuf::from(launch["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        workspace.path().canonicalize().unwrap()
    );
    assert_eq!(
        PathBuf::from(launch["home"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        workspace.path().canonicalize().unwrap(),
        "运行环境的家目录限定在工作目录"
    );
    let args: Vec<&str> = launch["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(&args[..3], ["-Xmx512m", "-Djava.awt.headless=true", "-jar"]);

    // 类型写错：仍显示已加载，但有警告且类型探测不符。
    let fallback = draft(
        "type: NotAWall\nhealth: 520\n",
        &[("class", "Wall"), ("health", "520")],
    );
    let workspace = tempfile::tempdir().unwrap();
    let evidence = runner.run(&fallback, workspace.path()).await;
    assert_eq!(evidence.exit, RunExit::Completed);
    assert!(evidence.loaded);
    assert_eq!(evidence.probes[0].actual.as_deref(), Some("Block"));
    assert!(evidence.warnings[0].contains("No type 'NotAWall' found"));
    assert!(!evidence.verified(&fallback));

    // 引用不存在的物品：服务端报错退出，没有探测结果。
    let broken = draft(
        "type: Wall\nrequirements: [ unobtainium/6 ]\n",
        &[("exists", "true")],
    );
    let workspace = tempfile::tempdir().unwrap();
    let evidence = runner.run(&broken, workspace.path()).await;
    evidence.validate(&broken).unwrap();
    assert_eq!(evidence.exit, RunExit::Crashed);
    assert!(evidence.probes.is_empty() && !evidence.loaded);
    assert!(
        evidence
            .warnings
            .iter()
            .any(|warning| warning.contains("unobtainium"))
    );

    // 探测对象不是本模组内容、或文件不合规时，结构检查给出原因且不运行。
    let mut stray = good.clone();
    stray.probes[0].subject = "copper-wall".into();
    assert!(runner.check(&stray)[0].contains("不是本模组的内容"));
    let mut script = good.clone();
    script.files.push(ArtifactFile {
        path: "scripts/main.js".into(),
        content: "print(1)".into(),
    });
    assert!(runner.check(&script)[0].contains("不允许的文件"));
    assert_eq!(std::fs::read_to_string(&record).unwrap().lines().count(), 3);
}

#[test]
fn missing_or_non_jar_runtime_is_refused_before_any_run() {
    let directory = tempfile::tempdir().unwrap();
    let not_jar = directory.path().join("server.jar");
    std::fs::write(&not_jar, b"#!/bin/sh").unwrap();
    for path in [directory.path().join("missing.jar"), not_jar] {
        assert!(MindustryServerRunner::new(RuntimeCommand::default(), path).is_err());
    }
}
