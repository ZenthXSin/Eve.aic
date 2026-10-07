use crate::strict_json;
use eve_knowledge_api::*;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    runs: Vec<ResearchRun>,
    documents: Vec<SourceDocument>,
    entries: Vec<KnowledgeEntry>,
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredKnowledge {
    inner: Mutex<Inner>,
}

impl StoredKnowledge {
    pub(super) fn open(context: PluginContext) -> KnowledgeResult<Self> {
        let mut ledger = match context
            .state_get(KNOWLEDGE_STATE_KEY)
            .map_err(|_| KnowledgeError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                runs: vec![],
                documents: vec![],
                entries: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(KnowledgeError::CorruptState);
                }
                let value =
                    strict_json::from_slice(&bytes).map_err(|_| KnowledgeError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        KnowledgeError::UnsupportedVersion
                    } else {
                        KnowledgeError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| KnowledgeError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| KnowledgeError::CorruptState)?;
                ledger
            }
        };
        let mut interrupted = false;
        for run in &mut ledger.runs {
            if run.status == RunStatus::Running {
                run.status = RunStatus::Interrupted;
                interrupted = true;
            }
        }
        // 先保存中断结局再公开实例；不重放已开始的抓取或模型请求，也不伪造完成时间。
        if interrupted {
            let bytes = encode(&ledger)?;
            context
                .state_set(KNOWLEDGE_STATE_KEY, bytes)
                .map_err(|_| KnowledgeError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> KnowledgeResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| KnowledgeError::Unavailable)?;
        if inner.context.is_none() {
            return Err(KnowledgeError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> KnowledgeResult<KnowledgeSnapshot> {
        let inner = self.lock()?;
        Ok(KnowledgeSnapshot {
            runs: inner.ledger.runs.clone(),
            documents: inner.ledger.documents.clone(),
            entries: inner.ledger.entries.clone(),
        })
    }

    pub(super) fn begin(
        &self,
        topic: ResearchTopic,
        policy: &SourcePolicy,
        researcher_version: &str,
        now_ms: u64,
    ) -> KnowledgeResult<Option<ResearchRun>> {
        topic.validate()?;
        validate_id(researcher_version)?;
        if now_ms == 0 {
            return Err(KnowledgeError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let id = run_id(&topic.goal_id, topic.goal_revision);
        let runs = &inner.ledger.runs;
        // 同一修订只研究一次；每个目标的总次数持久计数，重启不会重置。
        if runs
            .iter()
            .any(|run| run.id == id || run.status == RunStatus::Running)
            || runs
                .iter()
                .filter(|run| run.topic.goal_id == topic.goal_id)
                .count()
                >= MAX_RUNS_PER_GOAL
        {
            return Ok(None);
        }
        if runs.len() >= MAX_RUNS {
            return Err(KnowledgeError::LimitReached);
        }
        let run = ResearchRun {
            id,
            topic,
            researcher_version: researcher_version.into(),
            seeds: policy.seeds().to_vec(),
            started_at_ms: now_ms,
            finished_at_ms: None,
            stage: ResearchStage::Discovering,
            status: RunStatus::Running,
            discovery: vec![],
            candidates: vec![],
            selected: vec![],
            fetches: vec![],
            output: None,
            results: vec![],
        };
        let mut next = inner.ledger.clone();
        next.runs.push(run.clone());
        persist(&mut inner, next)?;
        Ok(Some(run))
    }

    pub(super) fn advance(
        &self,
        run_id: &str,
        progress: ResearchProgress,
    ) -> KnowledgeResult<ResearchRun> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .runs
            .iter()
            .position(|run| run.id == run_id)
            .ok_or(KnowledgeError::NotFound)?;
        let mut run = next.runs[position].clone();
        if run.status != RunStatus::Running {
            return Err(KnowledgeError::Conflict);
        }
        let policy = SourcePolicy::new(&run.seeds).map_err(|_| KnowledgeError::CorruptState)?;
        match progress {
            ResearchProgress::Discovered {
                attempts,
                candidates,
            } => {
                if run.stage != ResearchStage::Discovering {
                    return Err(KnowledgeError::Conflict);
                }
                if attempts.len() != run.seeds.len()
                    || attempts
                        .iter()
                        .zip(&run.seeds)
                        .any(|(attempt, seed)| attempt.url != *seed)
                {
                    return Err(KnowledgeError::InvalidInput);
                }
                // 候选只能来自本次实际抓到的入口页面链接，不接受外部补充的 URL。
                let mut offered = BTreeSet::new();
                for attempt in &attempts {
                    if let Ok(page) = &attempt.result {
                        for link in &page.links {
                            offered.insert((link.url.clone(), link.text.clone()));
                        }
                    }
                }
                let mut seen = BTreeSet::new();
                if candidates.len() > MAX_CANDIDATES
                    || candidates.iter().any(|candidate| {
                        run.seeds.contains(&candidate.url)
                            || !seen.insert(candidate.url.as_str())
                            || !offered.contains(&(candidate.url.clone(), candidate.text.clone()))
                    })
                {
                    return Err(KnowledgeError::InvalidInput);
                }
                run.discovery =
                    record_attempts(&mut next.documents, &policy, run.started_at_ms, attempts)?;
                run.candidates = candidates;
                run.stage = ResearchStage::Selecting;
            }
            ResearchProgress::Selected { indices } => {
                if run.stage != ResearchStage::Selecting {
                    return Err(KnowledgeError::Conflict);
                }
                validate_selection(run.candidates.len(), &indices)?;
                run.selected = indices;
                run.stage = ResearchStage::Fetching;
            }
            ResearchProgress::Fetched { attempts } => {
                if run.stage != ResearchStage::Fetching {
                    return Err(KnowledgeError::Conflict);
                }
                if attempts.len() != run.selected.len()
                    || attempts
                        .iter()
                        .zip(&run.selected)
                        .any(|(attempt, index)| attempt.url != run.candidates[*index].url)
                {
                    return Err(KnowledgeError::InvalidInput);
                }
                run.fetches =
                    record_attempts(&mut next.documents, &policy, run.started_at_ms, attempts)?;
                run.stage = ResearchStage::Extracting;
            }
        }
        next.runs[position] = run.clone();
        persist(&mut inner, next)?;
        Ok(run)
    }

    pub(super) fn finish(
        &self,
        run_id: &str,
        at_ms: u64,
        outcome: ResearchOutcome,
    ) -> KnowledgeResult<ResearchRun> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let position = next
            .runs
            .iter()
            .position(|run| run.id == run_id)
            .ok_or(KnowledgeError::NotFound)?;
        let mut run = next.runs[position].clone();
        if run.status != RunStatus::Running {
            return Err(KnowledgeError::Conflict);
        }
        if at_ms < run.started_at_ms {
            return Err(KnowledgeError::InvalidInput);
        }
        match outcome {
            ResearchOutcome::Failed(failure) => run.status = RunStatus::Failed(failure),
            ResearchOutcome::Completed(output) => {
                if !output.is_empty() && run.stage != ResearchStage::Extracting {
                    return Err(KnowledgeError::InvalidInput);
                }
                let texts = fetched_texts(&run, &next.documents)?;
                validate_extraction(&texts, &output)?;
                let goal_id = run.topic.goal_id.clone();
                let mut drafts = Vec::new();
                for claim in &output.claims {
                    drafts.push((
                        claim_entry_id(&goal_id, &claim.document_id, &claim.quote),
                        KnowledgeKind::from(claim.kind),
                        KnowledgeStatus::SourceQuoted,
                        claim.statement.clone(),
                        claim.version.clone(),
                        Some(KnowledgeSource {
                            document_id: claim.document_id.clone(),
                            quote: claim.quote.clone(),
                        }),
                    ));
                }
                for hypothesis in &output.hypotheses {
                    drafts.push((
                        hypothesis_entry_id(&goal_id, &hypothesis.statement),
                        KnowledgeKind::Hypothesis,
                        KnowledgeStatus::Unverified,
                        hypothesis.statement.clone(),
                        None,
                        None,
                    ));
                }
                let mut results = Vec::with_capacity(drafts.len());
                for (id, kind, status, statement, version, source) in drafts {
                    results.push(if next.entries.iter().any(|entry| entry.id == id) {
                        EntryResult::Duplicate { entry_id: id }
                    } else if next.entries.len() >= MAX_ENTRIES {
                        // 容量满不淘汰旧知识；原因随研究记录保存。
                        EntryResult::Rejected
                    } else {
                        next.entries.push(KnowledgeEntry {
                            id: id.clone(),
                            goal_id: goal_id.clone(),
                            owner: run.topic.owner.clone(),
                            run_id: run.id.clone(),
                            kind,
                            status,
                            statement,
                            version,
                            source,
                            created_at_ms: at_ms,
                        });
                        EntryResult::Created { entry_id: id }
                    });
                }
                run.output = Some(output);
                run.results = results;
                run.status = RunStatus::Completed;
            }
        }
        run.finished_at_ms = Some(at_ms);
        next.runs[position] = run.clone();
        persist(&mut inner, next)?;
        Ok(run)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("领域知识状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

/// 保存抓取记录；同一最终 URL 与内容的文档只保存一次，保留首次抓到的时间。
fn record_attempts(
    documents: &mut Vec<SourceDocument>,
    policy: &SourcePolicy,
    started_at_ms: u64,
    attempts: Vec<FetchAttempt>,
) -> KnowledgeResult<Vec<FetchRecord>> {
    let mut records = Vec::with_capacity(attempts.len());
    for attempt in attempts {
        if attempt.at_ms < started_at_ms || !policy.allows(&attempt.url) {
            return Err(KnowledgeError::InvalidInput);
        }
        let outcome = match attempt.result {
            Err(failure) => FetchOutcome::Failed { failure },
            Ok(page) => {
                page.validate(policy)?;
                let id = document_id(&page.final_url, &page.sha256);
                if !documents.iter().any(|document| document.id == id) {
                    if documents.len() >= MAX_DOCUMENTS {
                        return Err(KnowledgeError::LimitReached);
                    }
                    documents.push(SourceDocument {
                        id: id.clone(),
                        url: page.final_url,
                        fetched_at_ms: attempt.at_ms,
                        content_type: page.content_type,
                        sha256: page.sha256,
                        byte_count: page.byte_count,
                        title: page.title,
                        text: page.text,
                        text_truncated: page.text_truncated,
                    });
                }
                FetchOutcome::Fetched { document_id: id }
            }
        };
        records.push(FetchRecord {
            url: attempt.url,
            at_ms: attempt.at_ms,
            outcome,
        });
    }
    Ok(records)
}

/// 本次研究实际抓到的所选页面正文；提炼结果只能引用这些文档。
fn fetched_texts<'a>(
    run: &ResearchRun,
    documents: &'a [SourceDocument],
) -> KnowledgeResult<Vec<(&'a str, &'a str)>> {
    let mut texts = Vec::new();
    for record in &run.fetches {
        if let FetchOutcome::Fetched { document_id } = &record.outcome {
            let document = documents
                .iter()
                .find(|document| document.id == *document_id)
                .ok_or(KnowledgeError::CorruptState)?;
            texts.push((document.id.as_str(), document.text.as_str()));
        }
    }
    Ok(texts)
}

fn persist(inner: &mut Inner, next: Ledger) -> KnowledgeResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(KnowledgeError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(KNOWLEDGE_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(KnowledgeError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> KnowledgeResult<Vec<u8>> {
    if ledger.runs.len() > MAX_RUNS
        || ledger.documents.len() > MAX_DOCUMENTS
        || ledger.entries.len() > MAX_ENTRIES
    {
        return Err(KnowledgeError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| KnowledgeError::InvalidInput)?;
    // Interrupted 比 Running 多四个字节；写入时预留，保证下次启动能保存中断结局。
    let recovery_bytes = ledger
        .runs
        .iter()
        .filter(|run| run.status == RunStatus::Running)
        .count()
        * ("Interrupted".len() - "Running".len());
    if bytes.len().saturating_add(recovery_bytes) > MAX_STATE_BYTES {
        return Err(KnowledgeError::LimitReached);
    }
    Ok(bytes)
}

/// 启动时完整核对；任何不一致都拒绝打开并保留原字节。
fn validate_ledger(ledger: &Ledger) -> KnowledgeResult<()> {
    let invalid = || KnowledgeError::InvalidInput;
    if ledger.format_version != FORMAT_VERSION
        || ledger.runs.len() > MAX_RUNS
        || ledger.documents.len() > MAX_DOCUMENTS
        || ledger.entries.len() > MAX_ENTRIES
    {
        return Err(invalid());
    }
    let mut documents = BTreeMap::new();
    for document in &ledger.documents {
        document.validate()?;
        if documents.insert(document.id.as_str(), document).is_some() {
            return Err(invalid());
        }
    }
    let mut runs = BTreeMap::new();
    let mut per_goal: BTreeMap<&str, usize> = BTreeMap::new();
    let mut referenced = BTreeSet::new();
    let mut created = BTreeMap::new();
    let mut duplicates = Vec::new();
    let mut running = 0;
    for run in &ledger.runs {
        validate_run(run, &documents)?;
        if runs.insert(run.id.as_str(), run).is_some() {
            return Err(invalid());
        }
        let count = per_goal.entry(run.topic.goal_id.as_str()).or_default();
        *count += 1;
        if *count > MAX_RUNS_PER_GOAL {
            return Err(invalid());
        }
        if run.status == RunStatus::Running {
            running += 1;
        }
        for record in run.discovery.iter().chain(&run.fetches) {
            if let FetchOutcome::Fetched { document_id } = &record.outcome {
                referenced.insert(document_id.as_str());
            }
        }
        for result in &run.results {
            match result {
                EntryResult::Created { entry_id } => {
                    if created.insert(entry_id.as_str(), run).is_some() {
                        return Err(invalid());
                    }
                }
                EntryResult::Duplicate { entry_id } => duplicates.push((entry_id.as_str(), run)),
                EntryResult::Rejected => {}
            }
        }
    }
    // 文档只经由研究写入；没有研究引用的文档说明状态被改写。
    if running > 1 || referenced.len() != documents.len() {
        return Err(invalid());
    }
    let mut entries = BTreeMap::new();
    for entry in &ledger.entries {
        entry.validate()?;
        let run = created.get(entry.id.as_str()).ok_or_else(invalid)?;
        if entries.insert(entry.id.as_str(), entry).is_some()
            || entry.run_id != run.id
            || entry.goal_id != run.topic.goal_id
            || entry.owner != run.topic.owner
            || entry.created_at_ms != run.finished_at_ms.unwrap_or_default()
        {
            return Err(invalid());
        }
        let output = run.output.as_ref().ok_or_else(invalid)?;
        let matches = match (&entry.source, entry.kind) {
            (None, KnowledgeKind::Hypothesis) => {
                entry.id == hypothesis_entry_id(&entry.goal_id, &entry.statement)
                    && output
                        .hypotheses
                        .iter()
                        .any(|hypothesis| hypothesis.statement == entry.statement)
            }
            (Some(source), kind) => {
                entry.id == claim_entry_id(&entry.goal_id, &source.document_id, &source.quote)
                    && output.claims.iter().any(|claim| {
                        KnowledgeKind::from(claim.kind) == kind
                            && claim.document_id == source.document_id
                            && claim.quote == source.quote
                            && claim.statement == entry.statement
                            && claim.version == entry.version
                    })
            }
            _ => false,
        };
        if !matches {
            return Err(invalid());
        }
    }
    if created.len() != entries.len() {
        return Err(invalid());
    }
    for (entry_id, run) in duplicates {
        let entry = entries.get(entry_id).ok_or_else(invalid)?;
        if entry.goal_id != run.topic.goal_id || entry.run_id == run.id {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_run(
    run: &ResearchRun,
    documents: &BTreeMap<&str, &SourceDocument>,
) -> KnowledgeResult<()> {
    let invalid = || KnowledgeError::InvalidInput;
    run.topic.validate()?;
    validate_id(&run.researcher_version)?;
    let policy = SourcePolicy::new(&run.seeds)?;
    if run.id != run_id(&run.topic.goal_id, run.topic.goal_revision)
        || policy.seeds() != run.seeds.as_slice()
        || run.started_at_ms == 0
    {
        return Err(invalid());
    }
    match (run.status, run.finished_at_ms) {
        (RunStatus::Running | RunStatus::Interrupted, None) => {}
        (RunStatus::Completed | RunStatus::Failed(_), Some(at)) if at >= run.started_at_ms => {}
        _ => return Err(invalid()),
    }
    let stage = run.stage;
    if stage == ResearchStage::Discovering {
        if !run.discovery.is_empty() || !run.candidates.is_empty() {
            return Err(invalid());
        }
    } else if run.discovery.len() != run.seeds.len()
        || run
            .discovery
            .iter()
            .zip(&run.seeds)
            .any(|(record, seed)| record.url != *seed)
    {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    if run.candidates.len() > MAX_CANDIDATES
        || run.candidates.iter().any(|candidate| {
            !policy.allows(&candidate.url)
                || run.seeds.contains(&candidate.url)
                || !seen.insert(candidate.url.as_str())
                || candidate.text.len() > MAX_LABEL_BYTES
                || candidate.text.chars().any(char::is_control)
        })
    {
        return Err(invalid());
    }
    if stage < ResearchStage::Fetching {
        if !run.selected.is_empty() {
            return Err(invalid());
        }
    } else {
        validate_selection(run.candidates.len(), &run.selected)?;
    }
    if stage < ResearchStage::Extracting {
        if !run.fetches.is_empty() {
            return Err(invalid());
        }
    } else if run.fetches.len() != run.selected.len()
        || run
            .fetches
            .iter()
            .zip(&run.selected)
            .any(|(record, index)| record.url != run.candidates[*index].url)
    {
        return Err(invalid());
    }
    for record in run.discovery.iter().chain(&run.fetches) {
        if record.at_ms < run.started_at_ms || !policy.allows(&record.url) {
            return Err(invalid());
        }
        if let FetchOutcome::Fetched { document_id } = &record.outcome {
            let document = documents.get(document_id.as_str()).ok_or_else(invalid)?;
            if !policy.allows(&document.url) || document.fetched_at_ms > record.at_ms {
                return Err(invalid());
            }
        }
    }
    match (&run.output, run.status) {
        (Some(output), RunStatus::Completed) => {
            if (!output.is_empty() && stage != ResearchStage::Extracting)
                || run.results.len() != output.claims.len() + output.hypotheses.len()
            {
                return Err(invalid());
            }
            let mut texts = Vec::new();
            for record in &run.fetches {
                if let FetchOutcome::Fetched { document_id } = &record.outcome {
                    let document = documents.get(document_id.as_str()).ok_or_else(invalid)?;
                    texts.push((document.id.as_str(), document.text.as_str()));
                }
            }
            validate_extraction(&texts, output)?;
        }
        (None, RunStatus::Completed) => return Err(invalid()),
        (None, _) if run.results.is_empty() => {}
        _ => return Err(invalid()),
    }
    Ok(())
}
