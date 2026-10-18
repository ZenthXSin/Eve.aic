//! 本地目录实现：启动时一次性读入内存，之后只读，不跟随运行期新增或修改的文件。
use eve_reference_api::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// 至多装配的资料份数。
pub const MAX_ROOTS: usize = 8;
/// 单个文件的字节上限；更大的文件跳过（多为生成数据）。
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// 全部资料的字节上限；超过时拒绝启动，不静默截断。
pub const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
/// 全部资料的文件数上限。
pub const MAX_FILES: usize = 40_000;
/// 只收录这些文本扩展名。
const EXTENSIONS: &[&str] = &[
    "java",
    "kt",
    "kts",
    "md",
    "json",
    "hjson",
    "txt",
    "properties",
    "gradle",
    "glsl",
    "frag",
    "vert",
    "js",
];
const MAX_HITS_PER_FILE: usize = 8;
const MAX_HIT_TEXT: usize = 240;
const MAX_EXCERPT_BYTES: usize = 24 * 1024;

/// 一份待装配的资料：名称与本地目录。
#[derive(Clone, Debug)]
pub struct RootSpec {
    pub name: String,
    pub directory: PathBuf,
}

struct Document {
    text: String,
    lower: String,
}

struct LoadedRoot {
    name: String,
    revision: Option<String>,
    documents: BTreeMap<String, Document>,
}

pub struct LocalReferenceLibrary {
    roots: Vec<LoadedRoot>,
}

impl std::fmt::Debug for LocalReferenceLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.roots
                    .iter()
                    .map(|root| (&root.name, root.documents.len())),
            )
            .finish()
    }
}

impl LocalReferenceLibrary {
    /// 读入全部资料。名称重复、目录不可读、没有可检索文件或超过总量上限时返回原因。
    pub fn open(specs: &[RootSpec]) -> Result<Self, String> {
        if specs.is_empty() || specs.len() > MAX_ROOTS {
            return Err(format!("参考资料须为 1 至 {MAX_ROOTS} 份"));
        }
        let mut budget = Budget::default();
        let mut roots: Vec<LoadedRoot> = Vec::new();
        for spec in specs {
            if !valid_root_name(&spec.name) {
                return Err(format!("参考资料名 {} 无效", spec.name));
            }
            if roots.iter().any(|root| root.name == spec.name) {
                return Err(format!("参考资料名 {} 重复", spec.name));
            }
            let metadata = fs::symlink_metadata(&spec.directory)
                .map_err(|_| format!("参考资料 {} 的目录无法读取", spec.name))?;
            if !metadata.is_dir() {
                return Err(format!("参考资料 {} 不是目录", spec.name));
            }
            let mut documents = BTreeMap::new();
            collect(&spec.directory, "", &mut documents, &mut budget)
                .map_err(|error| format!("参考资料 {}：{error}", spec.name))?;
            if documents.is_empty() {
                return Err(format!("参考资料 {} 没有可检索的文本文件", spec.name));
            }
            roots.push(LoadedRoot {
                name: spec.name.clone(),
                revision: git_revision(&spec.directory),
                documents,
            });
        }
        Ok(Self { roots })
    }

    fn root(&self, name: &str) -> Result<&LoadedRoot, ReferenceError> {
        self.roots
            .iter()
            .find(|root| root.name == name)
            .ok_or_else(|| ReferenceError::UnknownRoot(name.into()))
    }
}

#[derive(Default)]
struct Budget {
    files: usize,
    bytes: u64,
}

fn collect(
    directory: &Path,
    prefix: &str,
    documents: &mut BTreeMap<String, Document>,
    budget: &mut Budget,
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .map_err(|_| format!("目录 {prefix} 无法读取"))?
        .collect::<Result<_, _>>()
        .map_err(|_| format!("目录 {prefix} 无法读取"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        // 隐藏目录（含 .git）与符号链接不收录，避免越出资料目录。
        if name.starts_with('.') {
            continue;
        }
        let metadata = entry
            .path()
            .symlink_metadata()
            .map_err(|_| format!("{prefix}{name} 无法读取"))?;
        let relative = format!("{prefix}{name}");
        if metadata.is_dir() {
            collect(&entry.path(), &format!("{relative}/"), documents, budget)?;
            continue;
        }
        let wanted = Path::new(&name)
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| EXTENSIONS.contains(&ext));
        if !metadata.is_file() || !wanted || metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        if !valid_relative_path(&relative) {
            continue;
        }
        let Ok(text) =
            String::from_utf8(fs::read(entry.path()).map_err(|_| format!("{relative} 无法读取"))?)
        else {
            continue;
        };
        budget.files += 1;
        budget.bytes += text.len() as u64;
        if budget.files > MAX_FILES || budget.bytes > MAX_TOTAL_BYTES {
            return Err(format!(
                "超过总量上限（{MAX_FILES} 个文件或 {} MiB）",
                MAX_TOTAL_BYTES / 1024 / 1024
            ));
        }
        let lower = text.to_lowercase();
        documents.insert(relative, Document { text, lower });
    }
    Ok(())
}

/// 读取目录下 git 检出的提交；只认 40 位十六进制的分离 HEAD 或直接引用文件。
fn git_revision(directory: &Path) -> Option<String> {
    let git = directory.join(".git");
    let head = fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let sha = match head.strip_prefix("ref: ") {
        Some(reference) if valid_relative_path(reference) => {
            fs::read_to_string(git.join(reference))
                .ok()?
                .trim()
                .to_owned()
        }
        Some(_) => return None,
        None => head.to_owned(),
    };
    (sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then_some(sha)
}

fn shorten(text: &str, limit: usize) -> String {
    clip(text.trim(), limit)
}

/// 按字节截断并保留缩进，超长时加省略号。
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.into();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

impl ReferenceLibrary for LocalReferenceLibrary {
    fn roots(&self) -> Vec<ReferenceRoot> {
        self.roots
            .iter()
            .map(|root| {
                let mut top_level: Vec<String> = root
                    .documents
                    .keys()
                    .filter_map(|path| path.split_once('/').map(|(head, _)| format!("{head}/")))
                    .collect();
                top_level.dedup();
                top_level.truncate(16);
                ReferenceRoot {
                    name: root.name.clone(),
                    revision: root.revision.clone(),
                    files: root.documents.len(),
                    top_level,
                }
            })
            .collect()
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchResult, ReferenceError> {
        request.validate()?;
        let roots: Vec<&LoadedRoot> = match &request.root {
            Some(name) => vec![self.root(name)?],
            None => self.roots.iter().collect(),
        };
        let query = request.query.trim().to_lowercase();
        let prefix = request.path_prefix.as_deref().unwrap_or("");
        let in_scope = || {
            roots.iter().flat_map(|root| {
                root.documents
                    .range(prefix.to_owned()..)
                    .take_while(|(path, _)| path.starts_with(prefix))
                    .map(move |(path, document)| (*root, path, document))
            })
        };
        let limit = request.max_results;
        let mut hits = Vec::new();
        // 文件名命中优先，便于直接定位类或文档；其后才是行内容命中。
        for (root, path, _) in in_scope() {
            if path.to_lowercase().contains(&query) {
                hits.push(SearchHit {
                    root: root.name.clone(),
                    path: path.clone(),
                    line: 0,
                    text: path.clone(),
                });
                if hits.len() > limit {
                    hits.truncate(limit);
                    return Ok(SearchResult {
                        hits,
                        truncated: true,
                    });
                }
            }
        }
        for (root, path, document) in in_scope() {
            if !document.lower.contains(&query) {
                continue;
            }
            let mut in_file = 0;
            for (index, (line, lower)) in document
                .text
                .lines()
                .zip(document.lower.lines())
                .enumerate()
            {
                if !lower.contains(&query) {
                    continue;
                }
                if hits.len() == limit {
                    return Ok(SearchResult {
                        hits,
                        truncated: true,
                    });
                }
                hits.push(SearchHit {
                    root: root.name.clone(),
                    path: path.clone(),
                    line: index + 1,
                    text: shorten(line, MAX_HIT_TEXT),
                });
                in_file += 1;
                if in_file == MAX_HITS_PER_FILE {
                    break;
                }
            }
        }
        Ok(SearchResult {
            hits,
            truncated: false,
        })
    }

    fn read(&self, request: &ReadRequest) -> Result<Excerpt, ReferenceError> {
        request.validate()?;
        let root = self.root(&request.root)?;
        let document = root
            .documents
            .get(&request.path)
            .ok_or_else(|| ReferenceError::NotFound(request.path.clone()))?;
        let total_lines = document.text.lines().count();
        if request.start_line > total_lines.max(1) {
            return Err(ReferenceError::Invalid(format!(
                "start_line 超出文件行数 {total_lines}"
            )));
        }
        let mut text = String::new();
        let mut end_line = request.start_line.saturating_sub(1);
        for (index, line) in document
            .text
            .lines()
            .enumerate()
            .skip(request.start_line - 1)
            .take(request.max_lines)
        {
            let numbered = format!("{}: {}\n", index + 1, clip(line, 2000));
            if !text.is_empty() && text.len() + numbered.len() > MAX_EXCERPT_BYTES {
                break;
            }
            text.push_str(&numbered);
            end_line = index + 1;
        }
        Ok(Excerpt {
            root: root.name.clone(),
            path: request.path.clone(),
            revision: root.revision.clone(),
            start_line: request.start_line,
            end_line,
            total_lines,
            text,
        })
    }
}
