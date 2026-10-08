//! 一次受控研究：抓取入口页面 → 选择候选 → 抓取所选页面 → 提炼知识。
//! 每个阶段先经账本保存，返回后才执行下一项外部动作；结局由宿主统一写入，
//! 以便停止、超时与存储失败都能留下明确记录。
use eve_knowledge_api::*;
use std::{collections::BTreeSet, sync::Arc};

pub struct Researcher {
    admin: Arc<dyn KnowledgeAdmin>,
    fetcher: Arc<dyn SourceFetcher>,
    selector: Arc<dyn SourceSelector>,
    extractor: Arc<dyn KnowledgeExtractor>,
}

impl Researcher {
    pub fn new(
        admin: Arc<dyn KnowledgeAdmin>,
        fetcher: Arc<dyn SourceFetcher>,
        selector: Arc<dyn SourceSelector>,
        extractor: Arc<dyn KnowledgeExtractor>,
    ) -> Self {
        Self {
            admin,
            fetcher,
            selector,
            extractor,
        }
    }

    /// 准入时随研究记录保存的版本。
    pub fn version(&self) -> String {
        format!("{}+{}", self.selector.version(), self.extractor.version())
    }

    /// 执行已保存为 Running 的研究直到得出结局；不调用 finish。
    /// 存储失败直接返回错误，宿主须停止并重新打开核对，不能继续使用旧缓存。
    pub async fn research(
        &self,
        run: &ResearchRun,
        now_ms: impl Fn() -> u64 + Send + Sync,
    ) -> KnowledgeResult<ResearchOutcome> {
        if run.status != RunStatus::Running || run.stage != ResearchStage::Discovering {
            return Err(KnowledgeError::Conflict);
        }
        let policy = SourcePolicy::new(&run.seeds)?;
        let mut attempts = Vec::with_capacity(run.seeds.len());
        for seed in &run.seeds {
            let at_ms = now_ms().max(run.started_at_ms);
            let result = self.fetcher.fetch(&policy, seed).await;
            attempts.push(FetchAttempt {
                url: seed.clone(),
                at_ms,
                result,
            });
        }
        let reachable = attempts.iter().any(|attempt| attempt.result.is_ok());
        let mut seen: BTreeSet<String> = run.seeds.iter().cloned().collect();
        let mut candidates = Vec::new();
        for attempt in &attempts {
            let Ok(page) = &attempt.result else {
                continue;
            };
            for link in &page.links {
                if candidates.len() == MAX_CANDIDATES {
                    break;
                }
                if seen.insert(link.url.clone()) {
                    candidates.push(LinkCandidate {
                        url: link.url.clone(),
                        text: link.text.clone(),
                    });
                }
            }
        }
        let run = match self.admin.advance(
            &run.id,
            ResearchProgress::Discovered {
                attempts,
                candidates,
            },
        ) {
            Ok(run) => run,
            Err(KnowledgeError::LimitReached) => {
                return Ok(ResearchOutcome::Failed(ResearchFailure::LimitReached));
            }
            Err(error) => return Err(error),
        };
        if !reachable {
            return Ok(ResearchOutcome::Failed(ResearchFailure::Fetch));
        }
        if run.candidates.is_empty() {
            return Ok(ResearchOutcome::Completed(ExtractionOutput::default()));
        }
        let request = SelectionRequest {
            run_id: run.id.clone(),
            selector_version: self.selector.version().into(),
            brief: run.topic.brief.clone(),
            brief_truncated: run.topic.brief_truncated,
            candidates: run
                .candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| SelectionCandidate {
                    index,
                    url: candidate.url.clone(),
                    text: candidate.text.clone(),
                })
                .collect(),
        };
        let indices = match self.selector.select(request).await {
            Ok(indices) => indices,
            Err(KnowledgeError::Research(failure)) => return Ok(ResearchOutcome::Failed(failure)),
            Err(error) => return Err(error),
        };
        if validate_selection(run.candidates.len(), &indices).is_err() {
            return Ok(ResearchOutcome::Failed(ResearchFailure::InvalidOutput));
        }
        let run = self
            .admin
            .advance(&run.id, ResearchProgress::Selected { indices })?;
        if run.selected.is_empty() {
            return Ok(ResearchOutcome::Completed(ExtractionOutput::default()));
        }
        let mut attempts = Vec::with_capacity(run.selected.len());
        for index in &run.selected {
            let url = &run.candidates[*index].url;
            let at_ms = now_ms().max(run.started_at_ms);
            let result = self.fetcher.fetch(&policy, url).await;
            attempts.push(FetchAttempt {
                url: url.clone(),
                at_ms,
                result,
            });
        }
        let run = match self
            .admin
            .advance(&run.id, ResearchProgress::Fetched { attempts })
        {
            Ok(run) => run,
            Err(KnowledgeError::LimitReached) => {
                return Ok(ResearchOutcome::Failed(ResearchFailure::LimitReached));
            }
            Err(error) => return Err(error),
        };
        let snapshot = self.admin.snapshot()?;
        let mut documents = Vec::new();
        for record in &run.fetches {
            let FetchOutcome::Fetched { document_id } = &record.outcome else {
                continue;
            };
            // 多个所选链接可能重定向到同一内容；同一文档只交给提炼器一次。
            if documents
                .iter()
                .any(|document: &ExtractionDocument| document.document_id == *document_id)
            {
                continue;
            }
            let document = snapshot
                .document(document_id)
                .ok_or(KnowledgeError::CorruptState)?;
            documents.push(ExtractionDocument {
                document_id: document.id.clone(),
                url: document.url.clone(),
                title: document.title.clone(),
                fetched_at_ms: document.fetched_at_ms,
                text: document.text.clone(),
                text_truncated: document.text_truncated,
            });
        }
        if documents.is_empty() {
            return Ok(ResearchOutcome::Failed(ResearchFailure::Fetch));
        }
        let request = ExtractionRequest {
            run_id: run.id.clone(),
            extractor_version: self.extractor.version().into(),
            brief: run.topic.brief.clone(),
            brief_truncated: run.topic.brief_truncated,
            documents,
        };
        Ok(match self.extractor.extract(request).await {
            Ok(output) => ResearchOutcome::Completed(output),
            Err(KnowledgeError::Research(failure)) => ResearchOutcome::Failed(failure),
            Err(error) => return Err(error),
        })
    }
}
