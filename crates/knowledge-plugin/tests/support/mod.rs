#![allow(dead_code)]
use eve_kernel::{Kernel, KernelServices, backends::FileStateStore};
use eve_knowledge_api::*;
use eve_knowledge_plugin::{KnowledgeController, KnowledgePlugin};
use eve_plugin_api::{PluginError, PluginId, PluginResult, StateStore};
use ring::digest::{SHA256, digest};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

pub const SEED: &str = "https://docs.example/wiki/modding/index.html";
pub const START: &str = "https://docs.example/wiki/modding/start.html";
pub const BLOCKS: &str = "https://docs.example/wiki/modding/blocks.html";
pub const START_TEXT: &str = "Mods are loaded from the mods folder. Each mod needs a mod.hjson file in its root. Tested with version 146.";
pub const BLOCKS_TEXT: &str = "Blocks are defined in content/blocks as hjson files.";

/// 真实文件库；提交前失败和提交成功但确认丢失分别注入。
pub struct RecordingStore {
    inner: FileStateStore,
    fail_before: AtomicBool,
    fail_after: AtomicBool,
    writes: AtomicUsize,
}
impl RecordingStore {
    pub fn open(path: &Path) -> Arc<Self> {
        Arc::new(Self {
            inner: FileStateStore::open(path).unwrap(),
            fail_before: AtomicBool::new(false),
            fail_after: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
        })
    }
    pub fn fail_before(&self, value: bool) {
        self.fail_before.store(value, Ordering::SeqCst);
    }
    pub fn fail_after(&self, value: bool) {
        self.fail_after.store(value, Ordering::SeqCst);
    }
    pub fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }
    pub fn stored(&self) -> Value {
        serde_json::from_slice(&self.raw().unwrap()).unwrap()
    }
    pub fn raw(&self) -> Option<Vec<u8>> {
        self.inner.get(&owner(), KNOWLEDGE_STATE_KEY).unwrap()
    }
    pub fn replace(&self, bytes: Vec<u8>) {
        self.inner
            .set(&owner(), KNOWLEDGE_STATE_KEY.into(), bytes)
            .unwrap();
    }
}
impl StateStore for RecordingStore {
    fn get(&self, namespace: &PluginId, key: &str) -> PluginResult<Option<Vec<u8>>> {
        self.inner.get(namespace, key)
    }
    fn set(&self, namespace: &PluginId, key: String, value: Vec<u8>) -> PluginResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_before.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        self.inner.set(namespace, key, value)?;
        if self.fail_after.load(Ordering::SeqCst) {
            return Err(PluginError::State("private-storage-detail".into()));
        }
        Ok(())
    }
}

pub fn owner() -> PluginId {
    PluginId::new(KNOWLEDGE_PLUGIN_ID).unwrap()
}

pub async fn open(state: Arc<dyn StateStore>) -> (Kernel, KnowledgeController) {
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let plugin = KnowledgePlugin::new().unwrap();
    let admin = plugin.controller();
    assert_eq!(admin.snapshot().err(), Some(KnowledgeError::Unavailable));
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    (kernel, admin)
}

pub fn policy() -> SourcePolicy {
    SourcePolicy::new(&[SEED.into()]).unwrap()
}

pub fn topic(goal: &str, revision: u64) -> ResearchTopic {
    ResearchTopic {
        goal_id: goal.into(),
        goal_revision: revision,
        owner: "user-a".into(),
        brief: "学习目标：Mindustry 模组创作；用户原话“我喜欢 Mindustry 这个游戏的模组”".into(),
        brief_truncated: false,
    }
}

pub fn sha(text: &str) -> String {
    digest(&SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn page(url: &str, text: &str, links: &[(&str, &str)]) -> FetchedPage {
    FetchedPage {
        final_url: url.into(),
        content_type: "text/html".into(),
        sha256: sha(text),
        byte_count: text.len() as u64,
        title: "Modding".into(),
        text: text.into(),
        text_truncated: false,
        links: links
            .iter()
            .map(|(url, text)| PageLink {
                url: (*url).into(),
                text: (*text).into(),
            })
            .collect(),
    }
}

pub fn index_page() -> FetchedPage {
    page(
        SEED,
        "Modding index",
        &[(START, "Getting started"), (BLOCKS, "Blocks")],
    )
}

pub fn attempt(url: &str, at_ms: u64, result: Result<FetchedPage, FetchFailure>) -> FetchAttempt {
    FetchAttempt {
        url: url.into(),
        at_ms,
        result,
    }
}

pub fn candidates() -> Vec<LinkCandidate> {
    vec![
        LinkCandidate {
            url: START.into(),
            text: "Getting started".into(),
        },
        LinkCandidate {
            url: BLOCKS.into(),
            text: "Blocks".into(),
        },
    ]
}

/// 依次推进到 Extracting：入口抓取、选择 start、抓取 start。
pub fn advance_to_extracting(admin: &dyn KnowledgeAdmin, run: &ResearchRun) -> ResearchRun {
    admin
        .advance(
            &run.id,
            ResearchProgress::Discovered {
                attempts: vec![attempt(SEED, run.started_at_ms, Ok(index_page()))],
                candidates: candidates(),
            },
        )
        .unwrap();
    admin
        .advance(&run.id, ResearchProgress::Selected { indices: vec![0] })
        .unwrap();
    admin
        .advance(
            &run.id,
            ResearchProgress::Fetched {
                attempts: vec![attempt(
                    START,
                    run.started_at_ms + 1,
                    Ok(page(START, START_TEXT, &[])),
                )],
            },
        )
        .unwrap()
}

pub fn start_document() -> String {
    document_id(START, &sha(START_TEXT))
}

pub fn claim(quote: &str, version: Option<&str>) -> ClaimDraft {
    ClaimDraft {
        document_id: start_document(),
        kind: ClaimKind::Procedure,
        statement: "每个模组根目录需要 mod.hjson".into(),
        quote: quote.into(),
        version: version.map(Into::into),
    }
}

pub fn output() -> ExtractionOutput {
    ExtractionOutput {
        claims: vec![claim(
            "Each mod needs a mod.hjson file in its root.",
            Some("146"),
        )],
        hypotheses: vec![HypothesisDraft {
            statement: "可能需要先建立 content 目录".into(),
        }],
    }
}

/// 按 URL 返回预设结果的抓取器；记录调用时账本是否已保存对应阶段。
pub struct FakeFetcher {
    pub pages: Mutex<BTreeMap<String, Result<FetchedPage, FetchFailure>>>,
    pub calls: Mutex<Vec<(String, Option<Value>)>>,
    pub store: Option<Arc<RecordingStore>>,
}
impl FakeFetcher {
    pub fn new(store: Option<Arc<RecordingStore>>) -> Arc<Self> {
        Arc::new(Self {
            pages: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(vec![]),
            store,
        })
    }
    pub fn serve(&self, url: &str, result: Result<FetchedPage, FetchFailure>) {
        self.pages.lock().unwrap().insert(url.into(), result);
    }
    pub fn urls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(url, _)| url.clone())
            .collect()
    }
}
impl SourceFetcher for FakeFetcher {
    fn fetch<'a>(
        &'a self,
        policy: &'a SourcePolicy,
        url: &'a str,
    ) -> BoxFuture<'a, Result<FetchedPage, FetchFailure>> {
        let saved = self
            .store
            .as_ref()
            .and_then(|store| store.raw())
            .map(|bytes| serde_json::from_slice(&bytes).unwrap());
        self.calls.lock().unwrap().push((url.into(), saved));
        let result = if policy.allows(url) {
            self.pages
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or(Err(FetchFailure::HttpStatus(404)))
        } else {
            Err(FetchFailure::NotAllowed)
        };
        Box::pin(async move { result })
    }
}

type Reply<T> = Box<dyn Fn(&T) -> KnowledgeResult<Value> + Send + Sync>;

/// 可编程选择器；记录请求与请求时的持久状态。
pub struct FakeSelector {
    pub reply: Mutex<Reply<SelectionRequest>>,
    pub requests: Mutex<Vec<(SelectionRequest, Option<Value>)>>,
    pub store: Option<Arc<RecordingStore>>,
}
impl FakeSelector {
    pub fn new(store: Option<Arc<RecordingStore>>, selected: Vec<usize>) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(Box::new(move |_| Ok(serde_json::json!(selected.clone())))),
            requests: Mutex::new(vec![]),
            store,
        })
    }
}
impl SourceSelector for FakeSelector {
    fn version(&self) -> &str {
        "fake-selector:v1"
    }
    fn select(&self, request: SelectionRequest) -> KnowledgeFuture<'_, Vec<usize>> {
        let saved = self
            .store
            .as_ref()
            .and_then(|store| store.raw())
            .map(|bytes| serde_json::from_slice(&bytes).unwrap());
        let reply = (self.reply.lock().unwrap())(&request)
            .map(|value| serde_json::from_value(value).unwrap());
        self.requests.lock().unwrap().push((request, saved));
        Box::pin(async move { reply })
    }
}

pub struct FakeExtractor {
    pub reply: Mutex<Reply<ExtractionRequest>>,
    pub requests: Mutex<Vec<(ExtractionRequest, Option<Value>)>>,
    pub store: Option<Arc<RecordingStore>>,
}
impl FakeExtractor {
    pub fn new(
        store: Option<Arc<RecordingStore>>,
        reply: impl Fn(&ExtractionRequest) -> KnowledgeResult<Value> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            reply: Mutex::new(Box::new(reply)),
            requests: Mutex::new(vec![]),
            store,
        })
    }
}
impl KnowledgeExtractor for FakeExtractor {
    fn version(&self) -> &str {
        "fake-extractor:v1"
    }
    fn extract(&self, request: ExtractionRequest) -> KnowledgeFuture<'_, ExtractionOutput> {
        let saved = self
            .store
            .as_ref()
            .and_then(|store| store.raw())
            .map(|bytes| serde_json::from_slice(&bytes).unwrap());
        let reply = (self.reply.lock().unwrap())(&request)
            .map(|value| serde_json::from_value(value).unwrap());
        self.requests.lock().unwrap().push((request, saved));
        Box::pin(async move { reply })
    }
}
