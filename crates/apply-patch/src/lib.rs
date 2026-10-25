//! 多文件补丁的解析与应用，格式与匹配规则沿用 openai/codex 的 apply_patch（Apache-2.0）：
//!
//! ```text
//! *** Begin Patch
//! *** Add File: 路径
//! +新文件的每一行
//! *** Delete File: 路径
//! *** Update File: 路径
//! *** Move to: 新路径          （可选）
//! @@ 用来定位的一行            （可选）
//!  上下文行
//! -删除的行
//! +新增的行
//! *** End of File              （可选，表示这一段在文件末尾）
//! *** End Patch
//! ```
//!
//! 这里只做纯文本计算，不读写文件；路径校验与提交由调用方负责。
//! 定位旧内容时依次尝试：完全一致、忽略行尾空白、忽略首尾空白、把各种破折号/引号/空格统一后比较。

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF: &str = "*** End of File";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Line {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Chunk {
    /// `@@ ` 之后用来缩小范围的一行（通常是函数或类型的定义）。
    pub context: Option<String>,
    pub lines: Vec<Line>,
    /// 这一段必须位于文件末尾。
    pub end_of_file: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}
impl Hunk {
    pub fn path(&self) -> &str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
    /// 这一段涉及的全部路径，包括移动的目标路径；授权检查要覆盖每一个。
    pub fn paths(&self) -> Vec<&str> {
        match self {
            Self::Update {
                path,
                move_to: Some(target),
                ..
            } => vec![path, target],
            other => vec![other.path()],
        }
    }
}

/// 补丁涉及的全部路径；补丁无法解析时为 None。
pub fn paths(patch: &str) -> Option<Vec<String>> {
    parse(patch).ok().map(|hunks| {
        hunks
            .iter()
            .flat_map(|h| {
                h.paths()
                    .into_iter()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect()
    })
}

fn error(line: usize, message: &str) -> String {
    format!("补丁第 {line} 行：{message}")
}

/// 解析补丁文本；容忍标记前后的空白，空行在更新段里按空的上下文行处理。
pub fn parse(patch: &str) -> Result<Vec<Hunk>, String> {
    let lines: Vec<&str> = patch.lines().collect();
    let first = lines
        .iter()
        .position(|l| !l.trim().is_empty())
        .ok_or("补丁为空")?;
    if lines[first].trim() != BEGIN {
        return Err(error(first + 1, "补丁必须以 *** Begin Patch 开头"));
    }
    let last = lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .expect("至少有开头一行");
    if lines[last].trim() != END || last == first {
        return Err(error(last + 1, "补丁必须以 *** End Patch 结尾"));
    }
    let body = &lines[first + 1..last];
    let mut hunks = Vec::new();
    let mut i = 0;
    let number = |i: usize| first + 2 + i;
    while i < body.len() {
        let line = body[i].trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }
        if let Some(path) = trimmed.strip_prefix(ADD) {
            let path = path.trim().to_string();
            let mut contents = String::new();
            i += 1;
            while i < body.len() && !body[i].trim_start().starts_with("*** ") {
                let text = body[i]
                    .strip_prefix('+')
                    .ok_or_else(|| error(number(i), "新文件的每一行必须以 + 开头"))?;
                contents.push_str(text);
                contents.push('\n');
                i += 1;
            }
            hunks.push(Hunk::Add { path, contents });
        } else if let Some(path) = trimmed.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete {
                path: path.trim().to_string(),
            });
            i += 1;
        } else if let Some(path) = trimmed.strip_prefix(UPDATE) {
            let path = path.trim().to_string();
            i += 1;
            let mut move_to = None;
            if let Some(target) = body
                .get(i)
                .and_then(|l| l.trim().strip_prefix(MOVE.trim_end()))
            {
                move_to = Some(target.trim().to_string());
                i += 1;
            }
            let mut chunks: Vec<Chunk> = Vec::new();
            while i < body.len() {
                let raw = body[i];
                let marker = raw.trim();
                if marker.starts_with("*** ") && marker != EOF {
                    break;
                }
                if marker == EOF {
                    chunks
                        .last_mut()
                        .ok_or_else(|| error(number(i), "*** End of File 之前没有修改内容"))?
                        .end_of_file = true;
                    i += 1;
                    continue;
                }
                if marker == "@@" || marker.starts_with("@@ ") {
                    let context = marker
                        .strip_prefix("@@")
                        .map(str::trim)
                        .filter(|c| !c.is_empty())
                        .map(str::to_string);
                    chunks.push(Chunk {
                        context,
                        ..Chunk::default()
                    });
                    i += 1;
                    continue;
                }
                if chunks.is_empty() {
                    chunks.push(Chunk::default());
                }
                let chunk = chunks.last_mut().expect("刚确保存在");
                let parsed = match raw.chars().next() {
                    Some(' ') => Line::Context(raw[1..].to_string()),
                    Some('-') => Line::Remove(raw[1..].to_string()),
                    Some('+') => Line::Add(raw[1..].to_string()),
                    None => {
                        // 段与段之间、补丁结尾前的空行只是分隔，不当作上下文。
                        let next = body[i + 1..]
                            .iter()
                            .map(|l| l.trim())
                            .find(|l| !l.is_empty());
                        if next.is_none_or(|l| {
                            l.starts_with("@@") || (l.starts_with("*** ") && l != EOF)
                        }) {
                            i += 1;
                            continue;
                        }
                        Line::Context(String::new())
                    }
                    Some(_) => {
                        return Err(error(
                            number(i),
                            "更新段的行必须以空格、- 或 + 开头（或是 @@ 定位行）",
                        ));
                    }
                };
                chunk.lines.push(parsed);
                i += 1;
            }
            chunks.retain(|c| !c.lines.is_empty());
            if chunks.is_empty() && move_to.is_none() {
                return Err(error(number(i), &format!("更新 {path} 没有任何修改")));
            }
            hunks.push(Hunk::Update {
                path,
                move_to,
                chunks,
            });
        } else {
            return Err(error(
                number(i),
                "需要 *** Add File:、*** Delete File: 或 *** Update File:",
            ));
        }
    }
    if hunks.is_empty() {
        return Err("补丁里没有任何文件操作".into());
    }
    Ok(hunks)
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{2018}'..='\u{201B}' => '\'',
            '\u{201C}'..='\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// 在 `lines` 中从 `start` 起找 `pattern`，逐级放宽比较方式。
fn seek(lines: &[String], pattern: &[String], start: usize, end_of_file: bool) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let from = if end_of_file {
        (lines.len() - pattern.len()).max(start)
    } else {
        start
    };
    let last = lines.len() - pattern.len();
    type Same<'a> = &'a dyn Fn(&str, &str) -> bool;
    let compare: [Same; 4] = [
        &|a, b| a == b,
        &|a, b| a.trim_end() == b.trim_end(),
        &|a, b| a.trim() == b.trim(),
        &|a, b| normalise(a) == normalise(b),
    ];
    compare.iter().find_map(|same| {
        (from..=last).find(|&i| {
            pattern
                .iter()
                .enumerate()
                .all(|(k, p)| same(&lines[i + k], p))
        })
    })
}

/// 把更新段应用到原文，返回新内容。上下文行保留原文本身（包括行尾），只替换删除与新增的行。
pub fn apply_update(original: &str, chunks: &[Chunk]) -> Result<String, String> {
    let newline = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let ends_with_newline = original.is_empty() || original.ends_with('\n');
    let mut lines: Vec<String> = original
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    let mut cursor = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        if let Some(context) = &chunk.context {
            let found = seek(&lines, std::slice::from_ref(context), cursor, false)
                .ok_or_else(|| format!("第 {} 段找不到定位行：{context}", index + 1))?;
            cursor = found + 1;
        }
        let old: Vec<String> = chunk
            .lines
            .iter()
            .filter_map(|l| match l {
                Line::Context(t) | Line::Remove(t) => Some(t.clone()),
                Line::Add(_) => None,
            })
            .collect();
        if old.is_empty() {
            // 只有新增行且没有上下文：照 Codex 的约定追加到文件末尾。
            let added = chunk.lines.iter().filter_map(|l| match l {
                Line::Add(t) => Some(t.clone()),
                _ => None,
            });
            lines.extend(added);
            cursor = lines.len();
            continue;
        }
        let start = seek(&lines, &old, cursor, chunk.end_of_file).ok_or_else(|| {
            format!(
                "第 {} 段找不到要修改的原文：\n{}",
                index + 1,
                old.join("\n")
            )
        })?;
        let mut replacement = Vec::new();
        let mut offset = 0;
        for line in &chunk.lines {
            match line {
                Line::Context(_) => {
                    replacement.push(lines[start + offset].clone());
                    offset += 1;
                }
                Line::Remove(_) => offset += 1,
                Line::Add(text) => replacement.push(text.clone()),
            }
        }
        let added = replacement.len();
        lines.splice(start..start + old.len(), replacement);
        cursor = start + added;
    }
    let mut text = lines.join(newline);
    if ends_with_newline && !lines.is_empty() {
        text.push_str(newline);
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_operations() {
        let patch = "*** Begin Patch\n*** Add File: docs/new.md\n+# 标题\n+\n+正文\n*** Delete File: old.txt\n*** Update File: src/lib.rs\n*** Move to: src/main.rs\n@@ fn main() {\n-    old();\n+    new();\n*** End of File\n*** End Patch\n";
        let hunks = parse(patch).unwrap();
        assert_eq!(
            hunks[0],
            Hunk::Add {
                path: "docs/new.md".into(),
                contents: "# 标题\n\n正文\n".into()
            }
        );
        assert_eq!(
            hunks[1],
            Hunk::Delete {
                path: "old.txt".into()
            }
        );
        let Hunk::Update {
            path,
            move_to,
            chunks,
        } = &hunks[2]
        else {
            panic!()
        };
        assert_eq!(
            (path.as_str(), move_to.as_deref()),
            ("src/lib.rs", Some("src/main.rs"))
        );
        assert_eq!(chunks[0].context.as_deref(), Some("fn main() {"));
        assert!(chunks[0].end_of_file);
        assert_eq!(
            chunks[0].lines,
            [
                Line::Remove("    old();".into()),
                Line::Add("    new();".into())
            ]
        );
    }

    #[test]
    fn blank_lines_between_hunks_are_separators() {
        let patch =
            "*** Begin Patch\n*** Update File: a\n-x\n+y\n\n*** Delete File: b\n\n*** End Patch";
        let hunks = parse(patch).unwrap();
        let Hunk::Update { chunks, .. } = &hunks[0] else {
            panic!()
        };
        assert_eq!(
            chunks[0].lines,
            [Line::Remove("x".into()), Line::Add("y".into())]
        );
        assert_eq!(hunks.len(), 2);
    }

    #[test]
    fn rejects_malformed_patches() {
        assert!(parse("").is_err());
        assert!(
            parse("*** Add File: a\n+x\n*** End Patch")
                .unwrap_err()
                .contains("Begin")
        );
        assert!(
            parse("*** Begin Patch\n*** Add File: a\n+x\n")
                .unwrap_err()
                .contains("End")
        );
        assert!(
            parse("*** Begin Patch\n*** Add File: a\nx\n*** End Patch")
                .unwrap_err()
                .contains("+ 开头")
        );
        assert!(parse("*** Begin Patch\n*** Rename: a\n*** End Patch").is_err());
        assert!(
            parse("*** Begin Patch\n*** Update File: a\n*** End Patch")
                .unwrap_err()
                .contains("没有任何修改")
        );
    }

    #[test]
    fn applies_chunks_in_order_and_keeps_original_context_lines() {
        let original = "fn a() {\n    one();\n}\n\nfn b() {\n    one();   \n}\n";
        let patch = "*** Begin Patch\n*** Update File: x.rs\n@@ fn b() {\n-    one();\n+    two();\n }\n*** End Patch";
        let Hunk::Update { chunks, .. } = &parse(patch).unwrap()[0] else {
            panic!()
        };
        // 定位行把范围缩到 fn b；原文行尾的空白不影响匹配，上下文行 `}` 保留原样。
        assert_eq!(
            apply_update(original, chunks).unwrap(),
            "fn a() {\n    one();\n}\n\nfn b() {\n    two();\n}\n"
        );
        let missing = vec![Chunk {
            context: None,
            lines: vec![Line::Remove("nope".into())],
            end_of_file: false,
        }];
        assert!(
            apply_update(original, &missing)
                .unwrap_err()
                .contains("找不到")
        );
    }

    #[test]
    fn crlf_files_stay_crlf_and_unicode_punctuation_still_matches() {
        let original = "a\r\nlet s = \u{201C}x\u{201D};\r\nb\r\n";
        let chunks = vec![Chunk {
            context: None,
            lines: vec![
                Line::Remove("let s = \"x\";".into()),
                Line::Add("let s = \"y\";".into()),
            ],
            end_of_file: false,
        }];
        assert_eq!(
            apply_update(original, &chunks).unwrap(),
            "a\r\nlet s = \"y\";\r\nb\r\n"
        );
        let append = vec![Chunk {
            context: None,
            lines: vec![Line::Add("c".into())],
            end_of_file: false,
        }];
        assert_eq!(apply_update("a\nb", &append).unwrap(), "a\nb\nc");
    }
}
