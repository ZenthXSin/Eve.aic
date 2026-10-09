//! 真实无头 Chromium 中的加载与探测；设置 EVE_TEST_BROWSER 后以 `--ignored` 运行。
use eve_practice_api::{ArtifactFile, PracticeDraft, PracticeRunner, Probe, RunExit};
use eve_practice_browser::BrowserRunner;
use std::path::PathBuf;

fn runner() -> BrowserRunner {
    let program = std::env::var_os("EVE_TEST_BROWSER").expect("EVE_TEST_BROWSER");
    BrowserRunner::new(PathBuf::from(program)).unwrap()
}

fn draft(css: &str, link: &str) -> PracticeDraft {
    let index = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><link rel=\"stylesheet\" href=\"{link}\"></head>\
         <body><h1 id=\"hero-title\">  欢迎来到\n我的主页 </h1><p id=\"intro\">你好</p></body></html>"
    );
    PracticeDraft {
        applicable: true,
        files: vec![
            ArtifactFile {
                path: "index.html".into(),
                content: index,
            },
            ArtifactFile {
                path: "css/site.css".into(),
                content: css.into(),
            },
        ],
        probes: vec![
            probe("hero-title", "fontSize", "40px"),
            probe("hero-title", "color", "rgb(200, 30, 30)"),
            probe("hero-title", "text", "欢迎来到 我的主页"),
            probe("intro", "exists", "true"),
        ],
        rationale: "一个带标题的主页".into(),
        notes_used: vec![],
    }
}

fn probe(subject: &str, property: &str, expected: &str) -> Probe {
    Probe {
        subject: subject.into(),
        property: property.into(),
        expected: expected.into(),
    }
}

#[tokio::test]
#[ignore]
async fn the_page_is_opened_in_a_real_browser_and_computed_styles_are_probed() {
    let runner = runner();
    let directory = tempfile::tempdir().unwrap();
    let good = draft(
        "h1 { font-size: 40px; color: rgb(200, 30, 30); }",
        "css/site.css",
    );
    assert_eq!(runner.check(&good), Vec::<String>::new());
    let evidence = runner.run(&good, directory.path()).await;
    println!("{}", evidence.log_excerpt);
    assert_eq!(evidence.exit, RunExit::Completed);
    assert!(evidence.loaded && evidence.warnings.is_empty());
    assert!(
        evidence.runtime_version.starts_with("Chromium "),
        "{}",
        evidence.runtime_version
    );
    assert!(evidence.probes.iter().all(|result| result.passed));
    assert!(
        !evidence
            .log_excerpt
            .contains(&directory.path().display().to_string())
    );

    // 实际值作为修正依据：字号写错时探测失败并给出浏览器计算的值。
    let wrong = draft(
        "h1 { font-size: 36px; color: rgb(200, 30, 30); }",
        "css/site.css",
    );
    let evidence = runner
        .run(&wrong, tempfile::tempdir().unwrap().path())
        .await;
    assert_eq!(evidence.exit, RunExit::Completed);
    assert_eq!(evidence.probes[0].actual.as_deref(), Some("36px"));
    assert!(!evidence.probes[0].passed && evidence.probes[1].passed);

    // 引用草稿中不存在的样式表，在运行前的检查中拒绝，不启动浏览器。
    let mut broken = good.clone();
    broken.files[1].path = "css/other.css".into();
    assert!(!runner.check(&broken).is_empty());
    let evidence = runner.run(&broken, &directory.path().join("again")).await;
    assert_eq!(evidence.exit, RunExit::StartFailed);
}
