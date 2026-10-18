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
