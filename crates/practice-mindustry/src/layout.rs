//! 草稿的结构检查：只允许 mod 清单与 content 目录下的数据文件，不允许脚本、代码或二进制内容。

/// 允许的内容目录；文件名去掉扩展名即内容名。
const CONTENT_DIRECTORIES: [&str; 4] = ["blocks", "items", "liquids", "units"];
/// mod 清单中会让游戏加载 Java 代码的字段。
const FORBIDDEN_MOD_KEYS: [&str; 2] = ["main", "java"];

pub(crate) struct Layout {
    pub mod_name: String,
    pub content: Vec<String>,
}

/// 检查文件布局并取出 mod 名称与内容名。返回 Err 时附带全部问题。
pub(crate) fn inspect(files: &[(&str, &str)]) -> Result<Layout, Vec<String>> {
    let mut issues = Vec::new();
    let mut manifest = None;
    let mut stems = Vec::new();
    for (path, content) in files {
        if *path == "mod.hjson" || *path == "mod.json" {
            if manifest.replace(*content).is_some() {
                issues.push("根目录只能有一个 mod.hjson 或 mod.json".to_string());
            }
        } else if let Some(stem) = content_stem(path) {
            stems.push(stem.to_string());
        } else {
            issues.push(format!(
                "不允许的文件：{path}；只能是根目录的 mod.hjson，或 content/blocks|items|liquids|units 下的 .hjson/.json"
            ));
            continue;
        }
        for (line, value) in values(content, "type") {
            if !is_type_name(&value) {
                issues.push(format!(
                    "{path} 第 {line} 行：type 只能是不带包名的类型名，实际为 {value}"
                ));
            }
        }
    }
    let mut mod_name = None;
    match manifest {
        None => issues.push("缺少根目录的 mod.hjson".into()),
        Some(manifest) => {
            for key in FORBIDDEN_MOD_KEYS {
                if !values(manifest, key).is_empty() {
                    issues.push(format!("mod 清单不得包含 {key} 字段；只允许纯数据内容"));
                }
            }
            match values(manifest, "name").first() {
                Some((_, name)) if is_content_name(name) => mod_name = Some(name.clone()),
                Some((_, name)) => issues.push(format!(
                    "mod 清单的 name 只能含小写字母、数字和 -，实际为 {name}"
                )),
                None => issues.push("mod 清单缺少 name".into()),
            }
            if values(manifest, "minGameVersion").is_empty() {
                issues.push("mod 清单缺少 minGameVersion".into());
            }
        }
    }
    if stems.is_empty() {
        issues.push("至少需要一个 content 数据文件".into());
    }
    match (issues.is_empty(), mod_name) {
        (true, Some(mod_name)) => Ok(Layout {
            content: stems
                .iter()
                .map(|stem| format!("{mod_name}-{stem}"))
                .collect(),
            mod_name,
        }),
        _ => Err(issues),
    }
}

fn content_stem(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("content/")?;
    let (directory, file) = rest.split_once('/')?;
    if !CONTENT_DIRECTORIES.contains(&directory) || file.contains('/') {
        return None;
    }
    let stem = file
        .strip_suffix(".hjson")
        .or_else(|| file.strip_suffix(".json"))?;
    is_content_name(stem).then_some(stem)
}

pub(crate) fn is_content_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_type_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic())
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// 取出文件中每一处 `key: value` 或 `"key": value` 的值，不论它在行首还是同一行的花括号里。
/// 只用于结构检查，不替代游戏本身的解析；宁可多查也不漏查。行号从 1 开始。
fn values(content: &str, key: &str) -> Vec<(usize, String)> {
    let bytes = content.as_bytes();
    let mut found = Vec::new();
    let mut start = 0;
    while let Some(offset) = content[start..].find(key) {
        let at = start + offset;
        start = at + key.len();
        let before = content[..at]
            .trim_end_matches('"')
            .as_bytes()
            .last()
            .copied();
        if before.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') {
            continue;
        }
        let rest = content[start..]
            .strip_prefix('"')
            .unwrap_or(&content[start..]);
        let Some(rest) = rest.trim_start_matches([' ', '\t']).strip_prefix(':') else {
            continue;
        };
        let rest = rest.trim_start_matches([' ', '\t']);
        let value = match rest.strip_prefix('"') {
            Some(quoted) => quoted.split('"').next().unwrap_or_default(),
            None => rest
                .split([',', '}', ']', '\n', '\r', '#'])
                .next()
                .unwrap_or_default()
                .trim(),
        };
        let line = bytes[..at].iter().filter(|byte| **byte == b'\n').count() + 1;
        found.push((line, value.to_string()));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "name: \"eve-sample\"\ndisplayName: \"Eve Sample\"\nversion: \"1.0\"\nminGameVersion: 146\n";

    #[test]
    fn accepts_data_only_mods_and_derives_content_names() {
        let layout = inspect(&[
            ("mod.hjson", MANIFEST),
            (
                "content/blocks/sample-wall.hjson",
                "type: Wall\nhealth: 520",
            ),
            (
                "content/items/dust.json",
                "{\"type\": \"Item\", \"cost\": 1}",
            ),
        ])
        .unwrap();
        assert_eq!(layout.mod_name, "eve-sample");
        assert_eq!(
            layout.content,
            ["eve-sample-sample-wall", "eve-sample-dust"]
        );
    }

    #[test]
    fn rejects_scripts_code_packages_and_bad_manifests_with_every_reason() {
        let issues = inspect(&[
            ("mod.hjson", "name: Eve_Sample\nmain: \"evil.Main\"\n"),
            ("scripts/main.js", "Vars.net.dispose()"),
            ("content/blocks/a.hjson", "type: java.lang.ProcessBuilder"),
            (
                "content/items/b.json",
                "{\"health\":1,\"type\":\"java.lang.Runtime\"}",
            ),
            (
                "content/units/c.hjson",
                "{weapons: [{bullet: {type: x.Y}}], type: UnitType}",
            ),
            ("content/blocks/Upper.hjson", "type: Wall"),
            ("content/sprites/x.hjson", "type: Wall"),
        ])
        .err()
        .unwrap();
        let joined = issues.join("\n");
        assert_eq!(
            issues
                .iter()
                .filter(|issue| issue.contains("type 只能是不带包名的类型名"))
                .count(),
            3,
            "行首、同一行 JSON 与嵌套对象中的 type 都要检查：\n{joined}"
        );
        for expected in [
            "不允许的文件：scripts/main.js",
            "type 只能是不带包名的类型名",
            "不允许的文件：content/blocks/Upper.hjson",
            "不允许的文件：content/sprites/x.hjson",
            "不得包含 main 字段",
            "name 只能含小写字母",
            "缺少 minGameVersion",
        ] {
            assert!(joined.contains(expected), "{expected}\n{joined}");
        }
        assert!(
            inspect(&[("content/blocks/a.hjson", "type: Wall")])
                .err()
                .unwrap()
                .contains(&"缺少根目录的 mod.hjson".to_string())
        );
        assert!(
            inspect(&[("mod.hjson", MANIFEST)])
                .err()
                .unwrap()
                .contains(&"至少需要一个 content 数据文件".to_string())
        );
    }
}
