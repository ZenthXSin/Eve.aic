use crate::strict_json;
use eve_plugin_api::{PluginContext, PluginError, PluginResult};
use eve_toolforge_api::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Mutex, MutexGuard},
};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    format_version: u32,
    tools: Vec<ForgedTool>,
    forges: Vec<ForgeAttempt>,
    calls: Vec<ToolCall>,
}
impl Ledger {
    fn snapshot(&self) -> ToolSnapshot {
        ToolSnapshot {
            tools: self.tools.clone(),
            forges: self.forges.clone(),
            calls: self.calls.clone(),
        }
    }
}
struct Inner {
    ledger: Ledger,
    context: Option<PluginContext>,
}
pub(super) struct StoredTools {
    inner: Mutex<Inner>,
}

impl StoredTools {
    pub(super) fn open(context: PluginContext) -> ToolResult<Self> {
        let mut ledger = match context
            .state_get(TOOLFORGE_STATE_KEY)
            .map_err(|_| ToolError::Storage)?
        {
            None => Ledger {
                format_version: FORMAT_VERSION,
                tools: vec![],
                forges: vec![],
                calls: vec![],
            },
            Some(bytes) => {
                if bytes.len() > MAX_STATE_BYTES {
                    return Err(ToolError::CorruptState);
                }
                let value = strict_json::from_slice(&bytes).map_err(|_| ToolError::CorruptState)?;
                if value.get("format_version").and_then(|value| value.as_u64())
                    != Some(u64::from(FORMAT_VERSION))
                {
                    return Err(if value.get("format_version").is_some() {
                        ToolError::UnsupportedVersion
                    } else {
                        ToolError::CorruptState
                    });
                }
                let ledger: Ledger =
                    serde_json::from_value(value).map_err(|_| ToolError::CorruptState)?;
                validate_ledger(&ledger).map_err(|_| ToolError::CorruptState)?;
                ledger
            }
        };
        let mut interrupted = false;
        for forge in &mut ledger.forges {
            if forge.status == ForgeStatus::Running {
                forge.status = ForgeStatus::Interrupted;
                interrupted = true;
            }
        }
        // 先保存中断结局再公开实例；不重放锻造请求，也不伪造完成时间。
        if interrupted {
            let bytes = encode(&ledger)?;
            context
                .state_set(TOOLFORGE_STATE_KEY, bytes)
                .map_err(|_| ToolError::Storage)?;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                ledger,
                context: Some(context),
            }),
        })
    }

    fn lock(&self) -> ToolResult<MutexGuard<'_, Inner>> {
        let inner = self.inner.lock().map_err(|_| ToolError::Unavailable)?;
        if inner.context.is_none() {
            return Err(ToolError::Unavailable);
        }
        Ok(inner)
    }

    pub(super) fn snapshot(&self) -> ToolResult<ToolSnapshot> {
        Ok(self.lock()?.ledger.snapshot())
    }

    /// 同一缺口同一出现次数只准入一次，每个缺口至多 `MAX_FORGES_PER_GAP` 次，同时只有一次锻造。
    fn admissible(ledger: &Ledger, owner: &str, gap: &GapRef, id: &str) -> bool {
        let same_gap = ledger
            .forges
            .iter()
            .filter(|forge| {
                forge.owner == owner
                    && forge.gap.runner_id == gap.runner_id
                    && forge.gap.key == gap.key
            })
            .count();
        same_gap < MAX_FORGES_PER_GAP
            && !ledger
                .forges
                .iter()
                .any(|forge| forge.id == id || forge.status == ForgeStatus::Running)
    }

    pub(super) fn begin_forge(
        &self,
        owner: &str,
        gap: GapRef,
        occurrences: usize,
        forger_version: &str,
        now_ms: u64,
    ) -> ToolResult<Option<ForgeAttempt>> {
        validate_id(owner)?;
        validate_id(forger_version)?;
        gap.validate()?;
        if now_ms == 0 || occurrences == 0 {
            return Err(ToolError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let id = forge_id(owner, &gap.runner_id, &gap.key, occurrences);
        let ledger = &inner.ledger;
        if !Self::admissible(ledger, owner, &gap, &id) {
            return Ok(None);
        }
        let tool = ledger
            .tools
            .iter()
            .find(|tool| tool.id == tool_id(owner, &gap.runner_id, &gap.key));
        // 已满版本的工具不再锻造；其余容量满时保留原记录并停止新的锻造。
        if tool.is_some_and(|tool| tool.versions.len() >= MAX_VERSIONS) {
            return Ok(None);
        }
        if ledger.forges.len() >= MAX_FORGES || (tool.is_none() && ledger.tools.len() >= MAX_TOOLS)
        {
            return Err(ToolError::LimitReached);
        }
        let attempt = ForgeAttempt {
            id,
            owner: owner.into(),
            gap,
            occurrences,
            forger_version: forger_version.into(),
            started_at_ms: now_ms,
            finished_at_ms: None,
            status: ForgeStatus::Running,
            output: None,
            verification: None,
            tool: None,
        };
        let mut next = inner.ledger.clone();
        next.forges.push(attempt.clone());
        persist(&mut inner, next)?;
        Ok(Some(attempt))
    }

    pub(super) fn record_forge(
        &self,
        id: &str,
        at_ms: u64,
        result: Result<ForgeOutput, ForgeFailure>,
        verification: Option<Verification>,
    ) -> ToolResult<ForgeAttempt> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let index = running(&next, id, at_ms)?;
        let mut entry = next.forges[index].clone();
        match result {
            Err(failure) => {
                if verification.is_some() {
                    return Err(ToolError::InvalidInput);
                }
                entry.status = ForgeStatus::Failed(failure);
            }
            Ok(ForgeOutput::NotForgeable { reason }) => {
                if verification.is_some() || !is_line(&reason, MAX_REASON_BYTES) {
                    return Err(ToolError::InvalidInput);
                }
                entry.output = Some(ForgeOutput::NotForgeable { reason });
                entry.status = ForgeStatus::NotForgeable;
            }
            Ok(ForgeOutput::Check(spec)) => {
                spec.validate()?;
                let verification = verification.ok_or(ToolError::InvalidInput)?;
                verification.validate()?;
                if verification.passed() {
                    let reference =
                        add_version(&mut next, &entry, spec.clone(), verification.clone(), at_ms)?;
                    entry.tool = Some(reference);
                    entry.status = ForgeStatus::Verified;
                } else {
                    entry.status = ForgeStatus::Rejected;
                }
                entry.output = Some(ForgeOutput::Check(spec));
                entry.verification = Some(verification);
            }
        }
        entry.finished_at_ms = Some(at_ms);
        next.forges[index] = entry.clone();
        persist(&mut inner, next)?;
        Ok(entry)
    }

    pub(super) fn record_reuse(
        &self,
        owner: &str,
        gap: GapRef,
        occurrences: usize,
        reference: ToolRef,
        verification: Verification,
        now_ms: u64,
    ) -> ToolResult<Option<ForgeAttempt>> {
        validate_id(owner)?;
        gap.validate()?;
        verification.validate()?;
        if now_ms == 0 || occurrences == 0 || !verification.passed() {
            return Err(ToolError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let id = forge_id(owner, &gap.runner_id, &gap.key, occurrences);
        let ledger = &inner.ledger;
        if !Self::admissible(ledger, owner, &gap, &id) {
            return Ok(None);
        }
        let tool = ledger
            .tools
            .iter()
            .find(|tool| tool.id == reference.tool_id)
            .ok_or(ToolError::NotFound)?;
        if tool.owner != owner
            || tool.runner_id != gap.runner_id
            || tool.enabled != Some(reference.version)
        {
            return Err(ToolError::Conflict);
        }
        if ledger.forges.len() >= MAX_FORGES {
            return Err(ToolError::LimitReached);
        }
        let attempt = ForgeAttempt {
            id,
            owner: owner.into(),
            gap,
            occurrences,
            forger_version: "reuse".into(),
            started_at_ms: now_ms,
            finished_at_ms: Some(now_ms),
            status: ForgeStatus::Reused,
            output: None,
            verification: Some(verification),
            tool: Some(reference),
        };
        let mut next = inner.ledger.clone();
        next.forges.push(attempt.clone());
        persist(&mut inner, next)?;
        Ok(Some(attempt))
    }

    pub(super) fn abandon_forge(
        &self,
        id: &str,
        at_ms: u64,
        failure: ForgeFailure,
    ) -> ToolResult<ForgeAttempt> {
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let index = running(&next, id, at_ms)?;
        let entry = &mut next.forges[index];
        entry.status = ForgeStatus::Failed(failure);
        entry.finished_at_ms = Some(at_ms);
        let entry = entry.clone();
        persist(&mut inner, next)?;
        Ok(entry)
    }

    pub(super) fn set_enabled(
        &self,
        tool_id: &str,
        at_ms: u64,
        actor: Actor,
        enabled: Option<u32>,
    ) -> ToolResult<ForgedTool> {
        if at_ms == 0 {
            return Err(ToolError::InvalidInput);
        }
        let mut inner = self.lock()?;
        let mut next = inner.ledger.clone();
        let tool = next
            .tools
            .iter_mut()
            .find(|tool| tool.id == tool_id)
            .ok_or(ToolError::NotFound)?;
        if enabled.is_some_and(|version| tool.version(version).is_none()) {
            return Err(ToolError::InvalidInput);
        }
        if tool.changes.len() >= MAX_CHANGES {
            return Err(ToolError::LimitReached);
        }
        if tool.changes.last().is_some_and(|last| at_ms < last.at_ms) {
            return Err(ToolError::Conflict);
        }
        tool.enabled = enabled;
        tool.changes.push(ToolChange {
            at_ms,
            actor,
            enabled,
        });
        let tool = tool.clone();
        persist(&mut inner, next)?;
        Ok(tool)
    }

    pub(super) fn record_call(&self, call: ToolCall) -> ToolResult<ToolCall> {
        let mut inner = self.lock()?;
        validate_call(&call, &inner.ledger.snapshot())?;
        let tool = inner
            .ledger
            .tools
            .iter()
            .find(|tool| tool.id == call.tool.tool_id)
            .ok_or(ToolError::NotFound)?;
        if tool.enabled != Some(call.tool.version) {
            return Err(ToolError::Conflict);
        }
        // 同一工具对同一次尝试只记录一次；完全相同的重复提交返回原记录。
        if let Some(existing) = inner.ledger.calls.iter().find(|entry| entry.id == call.id) {
            return if *existing == call {
                Ok(existing.clone())
            } else {
                Err(ToolError::Conflict)
            };
        }
        if inner.ledger.calls.len() >= MAX_CALLS {
            return Err(ToolError::LimitReached);
        }
        let mut next = inner.ledger.clone();
        next.calls.push(call.clone());
        persist(&mut inner, next)?;
        Ok(call)
    }

    pub(super) fn close(&self) -> PluginResult<()> {
        self.inner
            .lock()
            .map_err(|_| PluginError::State("工具锻造状态锁不可用".into()))?
            .context = None;
        Ok(())
    }
}

fn running(ledger: &Ledger, id: &str, at_ms: u64) -> ToolResult<usize> {
    let index = ledger
        .forges
        .iter()
        .position(|forge| forge.id == id)
        .ok_or(ToolError::NotFound)?;
    let entry = &ledger.forges[index];
    if entry.status != ForgeStatus::Running {
        return Err(ToolError::Conflict);
    }
    if at_ms < entry.started_at_ms {
        return Err(ToolError::InvalidInput);
    }
    Ok(index)
}

/// 验证通过的规格成为该缺口工具的新版本；用户没有停用时同一提交自动启用。
fn add_version(
    ledger: &mut Ledger,
    entry: &ForgeAttempt,
    spec: CheckSpec,
    verification: Verification,
    at_ms: u64,
) -> ToolResult<ToolRef> {
    let id = tool_id(&entry.owner, &entry.gap.runner_id, &entry.gap.key);
    let index = match ledger.tools.iter().position(|tool| tool.id == id) {
        Some(index) => index,
        None => {
            if ledger.tools.len() >= MAX_TOOLS {
                return Err(ToolError::LimitReached);
            }
            ledger.tools.push(ForgedTool {
                id: id.clone(),
                owner: entry.owner.clone(),
                runner_id: entry.gap.runner_id.clone(),
                gap_key: entry.gap.key.clone(),
                versions: vec![],
                enabled: None,
                changes: vec![],
            });
            ledger.tools.len() - 1
        }
    };
    let tool = &mut ledger.tools[index];
    if tool.versions.len() >= MAX_VERSIONS {
        return Err(ToolError::LimitReached);
    }
    let version = tool.latest().map_or(1, |latest| latest.version + 1);
    tool.versions.push(ToolVersion {
        version,
        forge_id: entry.id.clone(),
        spec,
        verification,
        created_at_ms: at_ms,
    });
    let after_last = tool.changes.last().is_none_or(|last| at_ms >= last.at_ms);
    if !tool.disabled_by_owner() && tool.changes.len() < MAX_CHANGES && after_last {
        tool.enabled = Some(version);
        tool.changes.push(ToolChange {
            at_ms,
            actor: Actor::Automatic,
            enabled: Some(version),
        });
    }
    Ok(ToolRef {
        tool_id: id,
        version,
    })
}

fn persist(inner: &mut Inner, next: Ledger) -> ToolResult<()> {
    let bytes = encode(&next)?;
    let context = inner.context.as_ref().ok_or(ToolError::Unavailable)?;
    // 失败也可能已经提交；关闭整个实例，禁止旧缓存继续读取或覆盖后端。
    if context.state_set(TOOLFORGE_STATE_KEY, bytes).is_err() {
        inner.context = None;
        return Err(ToolError::Storage);
    }
    inner.ledger = next;
    Ok(())
}

fn encode(ledger: &Ledger) -> ToolResult<Vec<u8>> {
    if ledger.tools.len() > MAX_TOOLS
        || ledger.forges.len() > MAX_FORGES
        || ledger.calls.len() > MAX_CALLS
    {
        return Err(ToolError::LimitReached);
    }
    let bytes = serde_json::to_vec(ledger).map_err(|_| ToolError::InvalidInput)?;
    // Interrupted 比 Running 多四个字节；写入时预留，保证下次启动能保存中断结局。
    let running = ledger
        .forges
        .iter()
        .filter(|forge| forge.status == ForgeStatus::Running)
        .count();
    let recovery = running * ("Interrupted".len() - "Running".len());
    if bytes.len().saturating_add(recovery) > MAX_STATE_BYTES {
        return Err(ToolError::LimitReached);
    }
    Ok(bytes)
}

fn validate_ledger(ledger: &Ledger) -> ToolResult<()> {
    if ledger.tools.len() > MAX_TOOLS
        || ledger.forges.len() > MAX_FORGES
        || ledger.calls.len() > MAX_CALLS
    {
        return Err(ToolError::CorruptState);
    }
    let snapshot = ledger.snapshot();
    let mut ids = BTreeSet::new();
    for forge in &ledger.forges {
        if !ids.insert(forge.id.as_str()) {
            return Err(ToolError::CorruptState);
        }
        validate_forge(forge, &snapshot)?;
    }
    if ledger
        .forges
        .iter()
        .filter(|forge| forge.status == ForgeStatus::Running)
        .count()
        > 1
    {
        return Err(ToolError::CorruptState);
    }
    for tool in &ledger.tools {
        if !ids.insert(tool.id.as_str()) {
            return Err(ToolError::CorruptState);
        }
        validate_tool(tool, &snapshot)?;
    }
    for call in &ledger.calls {
        if !ids.insert(call.id.as_str()) {
            return Err(ToolError::CorruptState);
        }
        validate_call(call, &snapshot)?;
    }
    Ok(())
}

fn validate_forge(forge: &ForgeAttempt, snapshot: &ToolSnapshot) -> ToolResult<()> {
    let corrupt = || ToolError::CorruptState;
    validate_id(&forge.owner)?;
    validate_id(&forge.forger_version)?;
    forge.gap.validate()?;
    if forge.occurrences == 0
        || forge.started_at_ms == 0
        || forge.id
            != forge_id(
                &forge.owner,
                &forge.gap.runner_id,
                &forge.gap.key,
                forge.occurrences,
            )
    {
        return Err(corrupt());
    }
    let finished = match forge.finished_at_ms {
        Some(at) if at >= forge.started_at_ms => true,
        Some(_) => return Err(corrupt()),
        None => false,
    };
    if let Some(verification) = &forge.verification {
        verification.validate()?;
    }
    let version = |reference: &ToolRef| {
        snapshot
            .tool(&reference.tool_id)
            .filter(|tool| tool.owner == forge.owner && tool.runner_id == forge.gap.runner_id)
            .and_then(|tool| tool.version(reference.version))
    };
    let passed = forge.verification.as_ref().map(Verification::passed);
    let valid = match (&forge.status, &forge.output, &forge.tool) {
        (ForgeStatus::Running, None, None) => !finished && forge.verification.is_none(),
        (ForgeStatus::Interrupted | ForgeStatus::Failed(_), None, None) => {
            forge.verification.is_none() && (finished || forge.status == ForgeStatus::Interrupted)
        }
        (ForgeStatus::NotForgeable, Some(ForgeOutput::NotForgeable { reason }), None) => {
            finished && forge.verification.is_none() && is_line(reason, MAX_REASON_BYTES)
        }
        (ForgeStatus::Rejected, Some(ForgeOutput::Check(spec)), None) => {
            finished && spec.validate().is_ok() && passed == Some(false)
        }
        (ForgeStatus::Verified, Some(ForgeOutput::Check(spec)), Some(reference)) => {
            finished
                && passed == Some(true)
                && version(reference).is_some_and(|entry| {
                    entry.forge_id == forge.id
                        && entry.spec == *spec
                        && Some(&entry.verification) == forge.verification.as_ref()
                })
        }
        (ForgeStatus::Reused, None, Some(reference)) => {
            finished && passed == Some(true) && version(reference).is_some()
        }
        _ => false,
    };
    if !valid {
        return Err(corrupt());
    }
    Ok(())
}

fn validate_tool(tool: &ForgedTool, snapshot: &ToolSnapshot) -> ToolResult<()> {
    let corrupt = || ToolError::CorruptState;
    validate_id(&tool.owner)?;
    validate_id(&tool.runner_id)?;
    validate_text(&tool.gap_key, eve_practice_api::MAX_ISSUE_BYTES)?;
    if tool.id != tool_id(&tool.owner, &tool.runner_id, &tool.gap_key)
        || tool.versions.is_empty()
        || tool.versions.len() > MAX_VERSIONS
        || tool.changes.len() > MAX_CHANGES
    {
        return Err(corrupt());
    }
    for (index, version) in tool.versions.iter().enumerate() {
        version.spec.validate()?;
        version.verification.validate()?;
        let forge = snapshot
            .forges
            .iter()
            .find(|forge| forge.id == version.forge_id);
        if version.version as usize != index + 1
            || !version.verification.passed()
            || forge.is_none_or(|forge| {
                forge.status != ForgeStatus::Verified
                    || forge.owner != tool.owner
                    || forge.gap.runner_id != tool.runner_id
                    || forge.gap.key != tool.gap_key
            })
        {
            return Err(corrupt());
        }
    }
    let exists =
        |version: Option<u32>| version.is_none_or(|version| tool.version(version).is_some());
    if !exists(tool.enabled)
        || tool
            .changes
            .windows(2)
            .any(|pair| pair[1].at_ms < pair[0].at_ms)
        || tool.changes.iter().any(|change| !exists(change.enabled))
        || tool.changes.last().map(|change| change.enabled) != Some(tool.enabled)
    {
        return Err(corrupt());
    }
    Ok(())
}

fn validate_call(call: &ToolCall, snapshot: &ToolSnapshot) -> ToolResult<()> {
    let invalid = || ToolError::InvalidInput;
    validate_id(&call.owner)?;
    validate_id(&call.run_id)?;
    validate_findings(&call.findings)?;
    if call.attempt == 0
        || call.at_ms == 0
        || call.id != call_id(&call.tool.tool_id, &call.run_id, call.attempt)
    {
        return Err(invalid());
    }
    let tool = snapshot
        .tool(&call.tool.tool_id)
        .ok_or(ToolError::NotFound)?;
    if tool.owner != call.owner || tool.version(call.tool.version).is_none() {
        return Err(invalid());
    }
    Ok(())
}
