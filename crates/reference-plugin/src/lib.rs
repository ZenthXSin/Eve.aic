//! 只读参考资料的实现层：本地目录资料库与对话工具适配。
//!
//! 工具只依赖 `eve-reference-api` 契约，可以替换为其他资料库实现。
mod local;
use eve_llm_api::*;
use eve_reference_api::*;
pub use local::{
    LocalReferenceLibrary, MAX_FILE_BYTES, MAX_FILES, MAX_ROOTS, MAX_TOTAL_BYTES, RootSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_SEARCH_RESULTS: usize = 20;
const DEFAULT_READ_LINES: usize = 120;

#[derive(Clone, Copy)]
enum Kind {
    Search,
    Read,
}

/// 为资料库生成 `search_reference` 与 `read_reference` 两个只读工具。
pub fn tools(library: Arc<dyn ReferenceLibrary>) -> Vec<Arc<dyn Tool>> {
    let catalog = catalog(&library.roots());
    [Kind::Search, Kind::Read]
        .into_iter()
        .map(|kind| {
            Arc::new(ReferenceTool {
                kind,
                library: library.clone(),
                catalog: catalog.clone(),
            }) as Arc<dyn Tool>
        })
        .collect()
}

/// 资料目录说明：名称、固定版本、文件数与顶层目录。
fn catalog(roots: &[ReferenceRoot]) -> String {
    roots
        .iter()
        .map(|root| {
            let revision = root.revision.as_deref().map_or_else(String::new, |sha| {
                format!("，提交 {}", &sha[..sha.len().min(10)])
            });
            let top = if root.top_level.is_empty() {
                String::new()
            } else {
                format!("，顶层 {}", root.top_level.join(" "))
            };
            format!("{}（{} 个文件{revision}{top}）", root.name, root.files)
        })
        .collect::<Vec<_>>()
        .join("；")
}

struct ReferenceTool {
    kind: Kind,
    library: Arc<dyn ReferenceLibrary>,
    catalog: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    root: String,
    path: String,
    #[serde(default)]
    start_line: Option<usize>,
    #[serde(default)]
    max_lines: Option<usize>,
}

enum Request {
    Search(SearchRequest),
    Read(ReadRequest),
}

fn invalid(error: impl std::fmt::Display) -> ToolValidationError {
    ToolValidationError {
        message: error.to_string(),
    }
}

impl ReferenceTool {
    fn request(&self, arguments: &Value) -> Result<Request, ToolValidationError> {
        match self.kind {
            Kind::Search => {
                let a: SearchArguments =
                    serde_json::from_value(arguments.clone()).map_err(invalid)?;
                let request = SearchRequest {
                    query: a.query,
                    root: a.root.filter(|root| !root.is_empty()),
                    path_prefix: a.path_prefix.filter(|prefix| !prefix.is_empty()),
                    max_results: a.max_results.unwrap_or(DEFAULT_SEARCH_RESULTS),
                };
                request.validate().map_err(invalid)?;
                Ok(Request::Search(request))
            }
            Kind::Read => {
                let a: ReadArguments =
                    serde_json::from_value(arguments.clone()).map_err(invalid)?;
                let request = ReadRequest {
                    root: a.root,
                    path: a.path,
                    start_line: a.start_line.unwrap_or(1),
                    max_lines: a.max_lines.unwrap_or(DEFAULT_READ_LINES),
                };
                request.validate().map_err(invalid)?;
                Ok(Request::Read(request))
            }
        }
    }
}

impl Tool for ReferenceTool {
    fn definition(&self) -> ToolDefinition {
        let (name, description, schema) = match self.kind {
            Kind::Search => (
                "search_reference",
                format!(
                    "在宿主装配的固定版本参考资料中按字面文本搜索（不区分大小写，不支持正则），先返回文件名命中，再返回“路径:行号”与该行原文。回答 Mindustry、Arc 的机制、字段、数值或 API 问题前先用它查，再用 read_reference 读上下文；没查到就如实说没查到。可用资料：{}。",
                    self.catalog
                ),
                json!({"type":"object","properties":{
                    "query":{"type":"string","description":"字面文本，例如类名 MendProjector、字段名 reload 或 JSON 键"},
                    "root":{"type":"string","description":"只搜索这份资料；省略则搜索全部"},
                    "path_prefix":{"type":"string","description":"只搜索以此开头的相对路径，例如 core/src/mindustry/world/"},
                    "max_results":{"type":"integer","minimum":1,"maximum":MAX_SEARCH_RESULTS}
                },"required":["query"],"additionalProperties":false}),
            ),
            Kind::Read => (
                "read_reference",
                format!(
                    "按行读取参考资料中的文件，返回带行号的原文、总行数与资料提交。引用结论时写“资料名/路径:行号”。每次至多 {MAX_READ_LINES} 行，需要更多就调大 start_line 继续读。可用资料：{}。",
                    self.catalog
                ),
                json!({"type":"object","properties":{
                    "root":{"type":"string"},
                    "path":{"type":"string","description":"资料内相对路径，取自 search_reference 的结果"},
                    "start_line":{"type":"integer","minimum":1},
                    "max_lines":{"type":"integer","minimum":1,"maximum":MAX_READ_LINES}
                },"required":["root","path"],"additionalProperties":false}),
            ),
        };
        ToolDefinition {
            name: name.into(),
            description,
            argument_schema: schema,
            required_permissions: vec![],
            concurrency: Some(ToolConcurrency::ParallelSafe),
        }
    }

    fn validate_arguments(&self, arguments: &Value) -> Result<(), ToolValidationError> {
        self.request(arguments).map(|_| ())
    }

    fn execute(&self, call: ToolCall, context: ToolExecutionContext) -> ToolFuture<'_> {
        Box::pin(async move {
            if context.is_cancel_requested() {
                return Err(ToolExecutionError::Cancelled(
                    "当前轮次已取消，未执行工具".into(),
                ));
            }
            let request = self
                .request(&call.arguments)
                .map_err(|error| ToolExecutionError::Failed(error.message))?;
            let result = match request {
                Request::Search(request) => self.library.search(&request).map(serde_json::to_value),
                Request::Read(request) => self.library.read(&request).map(serde_json::to_value),
            };
            result
                .map_err(|error| ToolExecutionError::Failed(error.to_string()))?
                .map_err(|_| ToolExecutionError::Failed("工具结果编码失败".into()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn library() -> (tempfile::TempDir, Arc<dyn ReferenceLibrary>) {
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("game");
        fs::create_dir_all(game.join("core/src/blocks")).unwrap();
        fs::write(
            game.join("core/src/blocks/MendProjector.java"),
            "public class MendProjector {\n    public float reload = 250f;\n    public float range = 60f;\n}\n",
        )
        .unwrap();
        fs::write(
            game.join("core/src/Vars.java"),
            "class Vars { int RELOAD; }\n",
        )
        .unwrap();
        fs::write(game.join("core/src/blob.bin"), [0u8, 1, 2]).unwrap();
        fs::create_dir_all(game.join(".git")).unwrap();
        fs::write(game.join(".git/HEAD"), format!("{}\n", "a".repeat(40))).unwrap();
        fs::write(game.join(".git/config.json"), "{\"secret\":1}").unwrap();
        let notes = dir.path().join("notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("mend.md"), "# 修复投影\n回复间隔见 reload。\n").unwrap();
        let library = LocalReferenceLibrary::open(&[
            RootSpec {
                name: "game".into(),
                directory: game,
            },
            RootSpec {
                name: "notes".into(),
                directory: notes,
            },
        ])
        .unwrap();
        (dir, Arc::new(library))
    }

    fn search(library: &dyn ReferenceLibrary, query: &str) -> SearchResult {
        library
            .search(&SearchRequest {
                query: query.into(),
                root: None,
                path_prefix: None,
                max_results: 20,
            })
            .unwrap()
    }

    #[test]
    fn loads_text_files_with_revision_and_skips_hidden_and_binary() {
        let (_dir, library) = library();
        let roots = library.roots();
        assert_eq!(roots[0].name, "game");
        assert_eq!(roots[0].revision.as_deref(), Some("a".repeat(40).as_str()));
        assert_eq!(roots[0].files, 2);
        assert_eq!(roots[0].top_level, vec!["core/"]);
        assert_eq!((roots[1].revision.clone(), roots[1].files), (None, 1));
        assert!(search(library.as_ref(), "secret").hits.is_empty());
    }

    #[test]
    fn search_ranks_file_names_first_and_ignores_case() {
        let (_dir, library) = library();
        let result = search(library.as_ref(), "mendprojector");
        assert_eq!(result.hits[0].line, 0);
        assert_eq!(result.hits[0].path, "core/src/blocks/MendProjector.java");
        assert_eq!(result.hits[1].line, 1);
        let reload = search(library.as_ref(), "Reload");
        let found: Vec<_> = reload
            .hits
            .iter()
            .map(|hit| (hit.root.as_str(), hit.path.as_str(), hit.line))
            .collect();
        assert_eq!(
            found,
            vec![
                ("game", "core/src/Vars.java", 1),
                ("game", "core/src/blocks/MendProjector.java", 2),
                ("notes", "mend.md", 2)
            ]
        );
        let scoped = library
            .search(&SearchRequest {
                query: "reload".into(),
                root: Some("game".into()),
                path_prefix: Some("core/src/blocks/".into()),
                max_results: 1,
            })
            .unwrap();
        assert_eq!((scoped.hits.len(), scoped.truncated), (1, false));
        let capped = library
            .search(&SearchRequest {
                query: "reload".into(),
                root: None,
                path_prefix: None,
                max_results: 2,
            })
            .unwrap();
        assert_eq!((capped.hits.len(), capped.truncated), (2, true));
        assert!(matches!(
            library.search(&SearchRequest {
                query: "x".into(),
                root: Some("missing".into()),
                path_prefix: None,
                max_results: 5,
            }),
            Err(ReferenceError::UnknownRoot(_))
        ));
    }

    #[test]
    fn read_returns_numbered_lines_with_indentation() {
        let (_dir, library) = library();
        let excerpt = library
            .read(&ReadRequest {
                root: "game".into(),
                path: "core/src/blocks/MendProjector.java".into(),
                start_line: 2,
                max_lines: 2,
            })
            .unwrap();
        assert_eq!(
            excerpt.text,
            "2:     public float reload = 250f;\n3:     public float range = 60f;\n"
        );
        assert_eq!((excerpt.end_line, excerpt.total_lines), (3, 4));
        assert_eq!(excerpt.revision.as_deref(), Some("a".repeat(40).as_str()));
        for (path, start) in [("core/src/blocks/MendProjector.java", 9), ("nope.java", 1)] {
            assert!(
                library
                    .read(&ReadRequest {
                        root: "game".into(),
                        path: path.into(),
                        start_line: start,
                        max_lines: 5,
                    })
                    .is_err()
            );
        }
    }

    #[test]
    fn open_rejects_bad_specs() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();
        let spec = |name: &str, directory: &std::path::Path| RootSpec {
            name: name.into(),
            directory: directory.into(),
        };
        for (specs, expected) in [
            (vec![], "1 至"),
            (vec![spec("Bad", dir.path())], "无效"),
            (vec![spec("empty", &empty)], "没有可检索"),
            (vec![spec("gone", &dir.path().join("gone"))], "无法读取"),
        ] {
            let error = LocalReferenceLibrary::open(&specs).unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
        fs::write(empty.join("a.md"), "x").unwrap();
        let error =
            LocalReferenceLibrary::open(&[spec("a", &empty), spec("a", &empty)]).unwrap_err();
        assert!(error.contains("重复"), "{error}");
    }

    #[tokio::test]
    async fn tools_validate_arguments_and_return_results() {
        let (_dir, library) = library();
        let tools = tools(library);
        let names: Vec<_> = tools.iter().map(|tool| tool.definition().name).collect();
        assert_eq!(names, ["search_reference", "read_reference"]);
        assert!(
            tools[0]
                .definition()
                .description
                .contains("game（2 个文件，提交 aaaaaaaaaa，顶层 core/）")
        );
        assert!(
            tools[0]
                .validate_arguments(&json!({"query":"reload"}))
                .is_ok()
        );
        for bad in [
            json!({}),
            json!({"query":"reload","owner":"x"}),
            json!({"query":"reload","max_results":500}),
            json!({"query":"reload","path_prefix":"../etc"}),
        ] {
            assert!(tools[0].validate_arguments(&bad).is_err(), "{bad}");
        }
        assert!(
            tools[1]
                .validate_arguments(&json!({"root":"game","path":"/etc/passwd"}))
                .is_err()
        );
        let call = |name: &str, arguments| ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments,
        };
        let hits = tools[0]
            .execute(
                call("search_reference", json!({"query":"range"})),
                ToolExecutionContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(hits["hits"][0]["line"], 3);
        let excerpt = tools[1]
            .execute(
                call("read_reference", json!({"root":"notes","path":"mend.md"})),
                ToolExecutionContext::new(),
            )
            .await
            .unwrap();
        assert_eq!(excerpt["text"], "1: # 修复投影\n2: 回复间隔见 reload。\n");
        let missing = tools[1]
            .execute(
                call("read_reference", json!({"root":"notes","path":"none.md"})),
                ToolExecutionContext::new(),
            )
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("资料中没有 none.md"));
    }
}

#[cfg(test)]
mod real {
    use super::*;
    use std::time::Instant;

    /// 用真实源码检出核对加载与搜索耗时：EVE_REFERENCE_DIRS=名称=目录,名称=目录。
    #[test]
    #[ignore]
    fn real_checkouts_load_and_search_quickly() {
        let specs: Vec<_> = std::env::var("EVE_REFERENCE_DIRS")
            .expect("EVE_REFERENCE_DIRS")
            .split(',')
            .map(|item| {
                let (name, directory) = item.split_once('=').unwrap();
                RootSpec {
                    name: name.into(),
                    directory: directory.into(),
                }
            })
            .collect();
        let started = Instant::now();
        let library = LocalReferenceLibrary::open(&specs).unwrap();
        eprintln!("load {:?} {:?}", started.elapsed(), library.roots());
        for query in ["MendProjector", "reload", "ContentParser", "consumePower"] {
            let started = Instant::now();
            let result = library
                .search(&SearchRequest {
                    query: query.into(),
                    root: None,
                    path_prefix: None,
                    max_results: 20,
                })
                .unwrap();
            eprintln!(
                "{query}: {:?} {} hits, first {:?}",
                started.elapsed(),
                result.hits.len(),
                result.hits.first()
            );
        }
    }
}
