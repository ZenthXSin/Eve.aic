//! 宿主随附的固定版本本地资料：按清单逐个核对摘要后整份载入内存，研究时只读这份载入的内容。
//! 地址形如 `eve-corpus://资料名/相对路径`；目录地址返回分页的资料目录，文件地址返回原文。
//! 只认清单列出的路径，不按请求拼接文件系统路径，也不读取清单之外的文件。
use crate::fetch::prefix;
use eve_knowledge_api::{
    BoxFuture, CORPUS_SCHEME, FetchFailure, FetchedPage, MAX_DOCUMENT_TEXT_BYTES, MAX_FETCH_BYTES,
    MAX_LABEL_BYTES, MAX_PAGE_LINKS, PageLink, SourceFetcher, SourcePolicy, canonical_url,
};
use ring::digest::{Context, SHA256};
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path, sync::Arc};

const MANIFEST: &str = "MANIFEST.json";
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_FILES: usize = 4096;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// 目录页每页列出的资料数；留一个位置给“下一页”。
const PAGE_SIZE: usize = MAX_PAGE_LINKS - 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format_version: u32,
    corpus: String,
    upstream: String,
    commit: String,
    license: String,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    sha256: String,
    bytes: u64,
}

struct CorpusFile {
    bytes: Vec<u8>,
    sha256: String,
    title: String,
}

/// 只读本地资料源。打开时核对清单中每个文件的长度与 SHA-256，任一不符即拒绝。
pub struct CorpusSourceFetcher {
    name: String,
    commit: String,
    files: BTreeMap<String, CorpusFile>,
}

impl CorpusSourceFetcher {
    pub fn open(directory: &Path) -> Result<Self, String> {
        let manifest_path = directory.join(MANIFEST);
        let size = std::fs::metadata(&manifest_path)
            .map_err(|_| "无法读取资料清单 MANIFEST.json".to_string())?
            .len();
        if size > MAX_MANIFEST_BYTES {
            return Err("资料清单超出上限".into());
        }
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(&manifest_path).map_err(|_| "无法读取资料清单 MANIFEST.json")?,
        )
        .map_err(|_| "资料清单格式无效")?;
        if manifest.format_version != 1
            || !valid_name(&manifest.corpus)
            || manifest.upstream.is_empty()
            || manifest.license.is_empty()
            || !(manifest.commit.len() == 40
                && manifest.commit.bytes().all(|b| b.is_ascii_hexdigit()))
            || manifest.files.is_empty()
            || manifest.files.len() > MAX_FILES
        {
            return Err("资料清单字段无效".into());
        }
        let mut files = BTreeMap::new();
        let mut total = 0u64;
        for entry in manifest.files {
            if !valid_path(&entry.path) || entry.bytes == 0 || entry.bytes > MAX_FETCH_BYTES {
                return Err(format!("资料清单中的路径或大小无效：{}", entry.path));
            }
            total += entry.bytes;
            if total > MAX_TOTAL_BYTES {
                return Err("资料总量超出上限".into());
            }
            let bytes = std::fs::read(directory.join(&entry.path))
                .map_err(|_| format!("无法读取资料：{}", entry.path))?;
            let sha256 = sha256(&bytes);
            if bytes.len() as u64 != entry.bytes || sha256 != entry.sha256 {
                return Err(format!("资料与清单不符：{}；不启用这份资料", entry.path));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| format!("资料不是 UTF-8 文本：{}", entry.path))?;
            let title = title(text, &entry.path);
            if files
                .insert(
                    entry.path.clone(),
                    CorpusFile {
                        bytes,
                        sha256,
                        title,
                    },
                )
                .is_some()
            {
                return Err(format!("资料清单路径重复：{}", entry.path));
            }
        }
        Ok(Self {
            name: manifest.corpus,
            commit: manifest.commit,
            files,
        })
    }

    /// 资料名，即地址中的主机部分。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 上游固定版本。
    pub fn commit(&self) -> &str {
        &self.commit
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    fn url(&self, path: &str) -> String {
        format!("{CORPUS_SCHEME}://{}/{path}", self.name)
    }

    fn page(&self, policy: &SourcePolicy, url: &str) -> Result<FetchedPage, FetchFailure> {
        if !policy.allows(url) {
            return Err(FetchFailure::NotAllowed);
        }
        let parsed = canonical_url(url).ok_or(FetchFailure::NotAllowed)?;
        if parsed.scheme() != CORPUS_SCHEME || parsed.host_str() != Some(self.name.as_str()) {
            return Err(FetchFailure::NotAllowed);
        }
        let path = parsed.path().trim_start_matches('/');
        if path.is_empty() || path.ends_with('/') {
            let page = match parsed.query() {
                None => 1,
                Some(query) => query
                    .strip_prefix("page=")
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n >= 1 && query == format!("page={n}"))
                    .ok_or(FetchFailure::NotAllowed)?,
            };
            return self.directory(policy, path, page, url);
        }
        if parsed.query().is_some() {
            return Err(FetchFailure::NotAllowed);
        }
        let file = self.files.get(path).ok_or(FetchFailure::HttpStatus(404))?;
        let text = std::str::from_utf8(&file.bytes).map_err(|_| FetchFailure::UnsupportedType)?;
        let kept = prefix(text, MAX_DOCUMENT_TEXT_BYTES);
        let mut links = Vec::new();
        for (target, label) in markdown_links(text) {
            if links.len() == MAX_PAGE_LINKS {
                break;
            }
            let Some(resolved) = resolve(path, &target) else {
                continue;
            };
            let link = self.url(&resolved);
            if self.files.contains_key(&resolved)
                && policy.allows(&link)
                && !links.iter().any(|known: &PageLink| known.url == link)
            {
                links.push(PageLink {
                    url: link,
                    text: label_text(&label, &self.files[&resolved].title),
                });
            }
        }
        finish(
            policy,
            FetchedPage {
                final_url: url.to_string(),
                content_type: "text/markdown; charset=utf-8".into(),
                sha256: file.sha256.clone(),
                byte_count: file.bytes.len() as u64,
                title: prefix(&file.title, MAX_LABEL_BYTES).to_string(),
                text: kept.to_string(),
                text_truncated: kept.len() < text.len(),
                links,
            },
        )
    }

    /// 目录页：按路径排序列出该目录及子目录中的全部资料，每页 PAGE_SIZE 条。
    fn directory(
        &self,
        policy: &SourcePolicy,
        directory: &str,
        page: usize,
        url: &str,
    ) -> Result<FetchedPage, FetchFailure> {
        let entries: Vec<_> = self
            .files
            .iter()
            .filter(|(path, _)| path.starts_with(directory))
            .collect();
        let pages = entries.len().div_ceil(PAGE_SIZE);
        if entries.is_empty() || page > pages {
            return Err(FetchFailure::HttpStatus(404));
        }
        let shown = &entries[(page - 1) * PAGE_SIZE..entries.len().min(page * PAGE_SIZE)];
        let shown_name = if directory.is_empty() { "/" } else { directory };
        let title = format!("资料目录：{shown_name}（第 {page}/{pages} 页）");
        let mut text = format!(
            "{title}\n来源：{}@{}，共 {} 篇。\n",
            self.name,
            self.commit,
            entries.len()
        );
        let mut links = Vec::with_capacity(shown.len() + 1);
        for (path, file) in shown {
            text.push_str(&format!("- {}（{path}）\n", file.title));
            links.push(PageLink {
                url: self.url(path),
                text: prefix(&file.title, MAX_LABEL_BYTES).to_string(),
            });
        }
        if page < pages {
            links.push(PageLink {
                url: format!("{}?page={}", self.url(directory), page + 1),
                text: format!("资料目录：{shown_name}（第 {} 页）", page + 1),
            });
        }
        let bytes = text.as_bytes();
        let kept = prefix(&text, MAX_DOCUMENT_TEXT_BYTES);
        finish(
            policy,
            FetchedPage {
                final_url: url.to_string(),
                content_type: "text/markdown; charset=utf-8".into(),
                sha256: sha256(bytes),
                byte_count: bytes.len() as u64,
                title,
                text: kept.to_string(),
                text_truncated: kept.len() < text.len(),
                links,
            },
        )
    }
}

impl std::fmt::Debug for CorpusSourceFetcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CorpusSourceFetcher")
            .field("name", &self.name)
            .field("commit", &self.commit)
            .field("files", &self.files.len())
            .finish()
    }
}

impl SourceFetcher for CorpusSourceFetcher {
    fn fetch<'a>(
        &'a self,
        policy: &'a SourcePolicy,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
        Box::pin(async move { self.page(policy, url) })
    }
}

/// 按地址方案分派：本地资料交给资料源，其他交给网页抓取器；没有装配资料源时本地地址不允许。
pub struct RoutingSourceFetcher {
    corpus: Option<Arc<CorpusSourceFetcher>>,
    web: Arc<dyn SourceFetcher>,
}

impl RoutingSourceFetcher {
    pub fn new(corpus: Option<Arc<CorpusSourceFetcher>>, web: Arc<dyn SourceFetcher>) -> Self {
        Self { corpus, web }
    }
}

impl SourceFetcher for RoutingSourceFetcher {
    fn fetch<'a>(
        &'a self,
        policy: &'a SourcePolicy,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
        if url.starts_with(&format!("{CORPUS_SCHEME}:")) {
            match &self.corpus {
                Some(corpus) => corpus.fetch(policy, url),
                None => Box::pin(async { Err(FetchFailure::NotAllowed) }),
            }
        } else {
            self.web.fetch(policy, url)
        }
    }
}

fn finish(policy: &SourcePolicy, page: FetchedPage) -> Result<FetchedPage, FetchFailure> {
    if page.text.trim().is_empty() {
        return Err(FetchFailure::Empty);
    }
    page.validate(policy).map_err(|_| FetchFailure::Empty)?;
    Ok(page)
}

fn sha256(bytes: &[u8]) -> String {
    let mut context = Context::new(&SHA256);
    context.update(bytes);
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// 清单路径只允许小写字母、数字与 `._-` 组成的相对路径段，以 `.md` 结尾。
fn valid_path(path: &str) -> bool {
    path.len() <= 512
        && path.ends_with(".md")
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
                })
        })
}

/// 第一行一级标题作为资料标题；没有时用路径。
fn title(text: &str, path: &str) -> String {
    text.lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty() && !title.chars().any(char::is_control))
        .unwrap_or(path)
        .to_string()
}

fn label_text(label: &str, fallback: &str) -> String {
    let label = label.trim();
    let text = if label.is_empty() || label.chars().any(char::is_control) {
        fallback
    } else {
        label
    };
    prefix(text, MAX_LABEL_BYTES).to_string()
}

/// markdown 行内链接 `[文字](目标)`；只取目标与文字，图片链接也按普通链接处理，后续按清单过滤。
fn markdown_links(text: &str) -> Vec<(String, String)> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let label = &after[..close];
        let target_start = &after[close + 2..];
        let Some(end) = target_start.find(')') else {
            break;
        };
        if !label.contains('\n') {
            links.push((target_start[..end].trim().to_string(), label.to_string()));
        }
        rest = &target_start[end..];
    }
    links
}

/// 把相对链接按当前文件所在目录解析为清单路径；带方案、绝对路径或越出资料根目录的链接返回 None。
fn resolve(current: &str, target: &str) -> Option<String> {
    let target = target.split('#').next()?.split(' ').next()?;
    if target.is_empty() || target.contains(':') || target.starts_with('/') {
        return None;
    }
    let mut segments: Vec<&str> = current.split('/').collect();
    segments.pop();
    for segment in target.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    let resolved = segments.join("/");
    valid_path(&resolved).then_some(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_corpus(directory: &Path, files: &[(&str, &str)]) {
        let entries: Vec<_> = files
            .iter()
            .map(|(path, text)| {
                let target = directory.join(path);
                std::fs::create_dir_all(target.parent().unwrap()).unwrap();
                std::fs::write(&target, text).unwrap();
                serde_json::json!({"path": path, "sha256": sha256(text.as_bytes()), "bytes": text.len()})
            })
            .collect();
        let manifest = serde_json::json!({
            "format_version": 1, "corpus": "eve-portable", "upstream": "https://example.invalid/eve",
            "commit": "e7e43b4db4de38775a8d28f59dd4ec69e5e1686e", "license": "AGPL-3.0", "files": entries});
        std::fs::write(directory.join(MANIFEST), manifest.to_string()).unwrap();
    }

    fn failure(result: Result<FetchedPage, FetchFailure>) -> FetchFailure {
        match result {
            Ok(page) => panic!("应当失败：{}", page.final_url),
            Err(failure) => failure,
        }
    }

    fn policy(seed: &str) -> SourcePolicy {
        SourcePolicy::new(&[seed.into()]).unwrap()
    }

    #[test]
    fn documents_keep_their_text_title_digest_and_in_scope_links() {
        let directory = tempfile::tempdir().unwrap();
        write_corpus(
            directory.path(),
            &[
                (
                    "mindustry/power.md",
                    "# 电力系统\n见 [单位](unit.md)、[加载](../concepts/load.md#x) 与 [外站](https://e.example/a) [缺失](gone.md)\n",
                ),
                ("mindustry/unit.md", "# 单位系统\n正文\n"),
                ("concepts/load.md", "# 加载链\n正文\n"),
            ],
        );
        let corpus = CorpusSourceFetcher::open(directory.path()).unwrap();
        assert_eq!((corpus.name(), corpus.len()), ("eve-portable", 3));
        let page = corpus
            .page(
                &policy("eve-corpus://eve-portable/"),
                "eve-corpus://eve-portable/mindustry/power.md",
            )
            .unwrap();
        assert_eq!(page.title, "电力系统");
        assert!(page.text.starts_with("# 电力系统") && !page.text_truncated);
        let links: Vec<_> = page
            .links
            .iter()
            .map(|l| (l.url.as_str(), l.text.as_str()))
            .collect();
        assert_eq!(
            links,
            [
                ("eve-corpus://eve-portable/mindustry/unit.md", "单位"),
                ("eve-corpus://eve-portable/concepts/load.md", "加载"),
            ]
        );
        // 入口目录之外的资料不作为链接给出，也不能直接读取。
        let scoped = policy("eve-corpus://eve-portable/mindustry/");
        let page = corpus
            .page(&scoped, "eve-corpus://eve-portable/mindustry/power.md")
            .unwrap();
        assert_eq!(page.links.len(), 1);
        assert_eq!(
            failure(corpus.page(&scoped, "eve-corpus://eve-portable/concepts/load.md")),
            FetchFailure::NotAllowed
        );
        assert_eq!(
            failure(corpus.page(&scoped, "eve-corpus://eve-portable/mindustry/missing.md")),
            FetchFailure::HttpStatus(404)
        );
    }

    #[test]
    fn directories_are_paginated_listings_within_the_link_limit() {
        let directory = tempfile::tempdir().unwrap();
        let files: Vec<(String, String)> = (0..70)
            .map(|i| {
                (
                    format!("concepts/c{i:03}.md"),
                    format!("# 概念 {i}\n正文\n"),
                )
            })
            .collect();
        let borrowed: Vec<_> = files
            .iter()
            .map(|(p, t)| (p.as_str(), t.as_str()))
            .collect();
        write_corpus(directory.path(), &borrowed);
        let corpus = CorpusSourceFetcher::open(directory.path()).unwrap();
        let seed = policy("eve-corpus://eve-portable/concepts/");
        let first = corpus
            .page(&seed, "eve-corpus://eve-portable/concepts/")
            .unwrap();
        assert_eq!(first.links.len(), MAX_PAGE_LINKS);
        assert_eq!(first.links[0].text, "概念 0");
        assert_eq!(
            first.links.last().unwrap().url,
            "eve-corpus://eve-portable/concepts/?page=2"
        );
        let second = corpus
            .page(&seed, "eve-corpus://eve-portable/concepts/?page=2")
            .unwrap();
        assert_eq!(second.links.len(), 70 - PAGE_SIZE);
        for invalid in ["?page=3", "?page=0", "?page=02", "?x=1"] {
            assert!(
                corpus
                    .page(
                        &seed,
                        &format!("eve-corpus://eve-portable/concepts/{invalid}")
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn tampered_missing_or_unsafe_entries_refuse_to_open() {
        let directory = tempfile::tempdir().unwrap();
        write_corpus(directory.path(), &[("mindustry/a.md", "# A\n正文\n")]);
        assert!(CorpusSourceFetcher::open(directory.path()).is_ok());
        std::fs::write(directory.path().join("mindustry/a.md"), "# A\n改过\n").unwrap();
        assert!(
            matches!(CorpusSourceFetcher::open(directory.path()), Err(error) if error.contains("不符"))
        );
        for path in ["../a.md", "mindustry/A.md", ".hidden/a.md", "a.txt"] {
            let directory = tempfile::tempdir().unwrap();
            write_corpus(directory.path(), &[("mindustry/a.md", "# A\n")]);
            let manifest = std::fs::read_to_string(directory.path().join(MANIFEST))
                .unwrap()
                .replace("mindustry/a.md", path);
            std::fs::write(directory.path().join(MANIFEST), manifest).unwrap();
            assert!(
                CorpusSourceFetcher::open(directory.path()).is_err(),
                "{path}"
            );
        }
        assert!(CorpusSourceFetcher::open(&directory.path().join("missing")).is_err());
    }

    #[tokio::test]
    async fn routing_sends_corpus_urls_to_the_corpus_and_refuses_them_without_one() {
        struct Web;
        impl SourceFetcher for Web {
            fn fetch<'a>(
                &'a self,
                _: &'a SourcePolicy,
                _: &'a str,
            ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
                Box::pin(async { Err(FetchFailure::Network) })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        write_corpus(directory.path(), &[("mindustry/a.md", "# A\n正文\n")]);
        let corpus = Arc::new(CorpusSourceFetcher::open(directory.path()).unwrap());
        let seeds = SourcePolicy::new(&[
            "eve-corpus://eve-portable/".into(),
            "https://docs.example/".into(),
        ])
        .unwrap();
        let routed = RoutingSourceFetcher::new(Some(corpus), Arc::new(Web));
        assert!(
            routed
                .fetch(&seeds, "eve-corpus://eve-portable/mindustry/a.md")
                .await
                .is_ok()
        );
        assert_eq!(
            failure(routed.fetch(&seeds, "https://docs.example/a").await),
            FetchFailure::Network
        );
        let bare = RoutingSourceFetcher::new(None, Arc::new(Web));
        assert_eq!(
            failure(
                bare.fetch(&seeds, "eve-corpus://eve-portable/mindustry/a.md")
                    .await
            ),
            FetchFailure::NotAllowed
        );
    }

    #[test]
    fn the_vendored_eve_portable_corpus_opens_and_every_directory_lists() {
        let directory =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/eve-portable-knowledge");
        let corpus = CorpusSourceFetcher::open(&directory).unwrap();
        assert_eq!(corpus.name(), "eve-portable");
        let all = policy("eve-corpus://eve-portable/");
        for directory in [
            "mindustry/",
            "mod-making-guide/",
            "concepts/",
            "memory/",
            "",
        ] {
            let page = corpus
                .page(&all, &format!("eve-corpus://eve-portable/{directory}"))
                .unwrap_or_else(|error| panic!("{directory}: {error:?}"));
            assert!(!page.links.is_empty());
        }
        let page = corpus
            .page(&all, "eve-corpus://eve-portable/mindustry/power-system.md")
            .unwrap();
        assert_eq!(page.title, "Mindustry 电力系统源码验证");
        // 导入规则已排除旧环境路径；这里再查一遍随仓库提供的副本，防止手工改动带入。
        for (path, file) in &corpus.files {
            let text = std::str::from_utf8(&file.bytes).unwrap();
            for marker in ["/home/", "/root/", "sshpass", "<SECRET>", "<SERVER_IP>"] {
                assert!(!text.contains(marker), "{path} 含有 {marker}");
            }
        }
    }
}
