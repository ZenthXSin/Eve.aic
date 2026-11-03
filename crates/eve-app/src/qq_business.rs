//! 业务授权账本。身份仍由通道/管理员目录确认；授权不改变安全审核与工具后端边界。
//! 原话、签发、撤销及审核结果一起原子持久化，提交不确定时本进程失败关闭。
use crate::AppError;
use eve_developer_api::ActionReviewVerdict;
use eve_llm_api::ContextScope;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

#[path = "qq_business_tool.rs"]
mod tool;
pub(crate) use tool::BusinessTool;

const MAX_BYTES: usize = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 4096;

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BusinessState {
    Unlocked,
    Revoked,
    EmergencyStopped,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Grant {
    pub scope: ContextScope,
    pub state: BusinessState,
    pub allowed_businesses: Vec<String>,
    pub cancellation_pending: bool,
    pub issued_by: String,
    pub issued_at_ms: u64,
    pub revoked_at_ms: Option<u64>,
    pub revision: u64,
    pub evidence_digest: String,
    pub evidence_turn_id: Option<u64>,
    /// 从压缩前历史复制的最近六条真实用户正文，保留完整文本。
    pub user_messages: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Audit {
    scope: ContextScope,
    actor: String,
    action: String,
    target: Option<String>,
    at_ms: u64,
    turn_id: Option<u64>,
    user_messages: Vec<String>,
    verdict: Option<ActionReviewVerdict>,
    outcome: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    revision: u64,
    grants: BTreeMap<String, Grant>,
    revoked_administrators: BTreeSet<String>,
    audit: Vec<Audit>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            grants: BTreeMap::new(),
            revoked_administrators: BTreeSet::new(),
            audit: Vec::new(),
        }
    }
}

fn key(scope: &ContextScope) -> String {
    digest(
        &SHA256,
        &serde_json::to_vec(scope).expect("可信范围可序列化"),
    )
    .as_ref()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect()
}

fn evidence(scope: &ContextScope, turn_id: Option<u64>, messages: &[String]) -> String {
    digest(
        &SHA256,
        &serde_json::to_vec(&(scope, turn_id, messages)).expect("授权证据可序列化"),
    )
    .as_ref()
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect()
}

fn valid_scope(scope: &ContextScope) -> bool {
    eve_session_api::SessionKey::new(&scope.session_id, &scope.user_id).is_ok()
        && scope.user_id != crate::qq_group::USER
}

fn valid_messages(messages: &[String]) -> bool {
    (1..=6).contains(&messages.len())
        && messages.iter().all(|message| !message.trim().is_empty())
        && messages.iter().map(String::len).sum::<usize>()
            <= eve_developer_api::MAX_ACTION_REVIEW_BYTES
}

impl Ledger {
    fn valid(&self) -> bool {
        self.version == 1
            && self.grants.len() <= MAX_RECORDS
            && self.revoked_administrators.len() <= MAX_RECORDS
            && self.audit.len() <= MAX_RECORDS
            && self.grants.iter().all(|(id, grant)| {
                id == &key(&grant.scope)
                    && valid_scope(&grant.scope)
                    && grant.issued_at_ms > 0
                    && grant.revision > 0
                    && grant.revision <= self.revision
                    && !grant.issued_by.is_empty()
                    && grant.allowed_businesses == ["all_business"]
                    && (!grant.cancellation_pending
                        || grant.state == BusinessState::EmergencyStopped)
                    && valid_messages(&grant.user_messages)
                    && grant.evidence_digest
                        == evidence(&grant.scope, grant.evidence_turn_id, &grant.user_messages)
                    && match grant.state {
                        BusinessState::Unlocked => grant.revoked_at_ms.is_none(),
                        _ => grant.revoked_at_ms.is_some_and(|t| t >= grant.issued_at_ms),
                    }
            })
            && self.audit.iter().all(|event| {
                valid_scope(&event.scope)
                    && event.at_ms > 0
                    && !event.actor.is_empty()
                    && !event.action.is_empty()
                    && valid_messages(&event.user_messages)
                    && !event.outcome.is_empty()
            })
            && self
                .revoked_administrators
                .iter()
                .all(|id| valid_role_id(id))
    }
}

pub(crate) fn valid_role_id(id: &str) -> bool {
    eve_qqbot_plugin::QqConversationIdentity::valid_account_id(id)
        || id.strip_prefix("legacy:").is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id.trim() == id
                && !id.chars().any(char::is_control)
        })
}

pub(crate) struct Store {
    path: Option<PathBuf>,
    state: Mutex<Ledger>,
    failed: AtomicBool,
    plan_scopes: Mutex<Option<PlanScopes>>,
}

struct PlanScopes {
    revision: u64,
    scopes: BTreeMap<String, ContextScope>,
}

pub(crate) struct Mutation {
    pub scope: ContextScope,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub at_ms: u64,
    pub turn_id: Option<u64>,
    pub user_messages: Vec<String>,
    pub verdict: Option<ActionReviewVerdict>,
    pub outcome: String,
    pub expected_revision: u64,
}

impl Store {
    pub(crate) fn open(directory: &Path) -> Result<Self, AppError> {
        let path = directory.join("business-permissions.json");
        let initialized = directory.join("business-permissions.initialized");
        match std::fs::symlink_metadata(directory.join("business-permissions.failed")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(
                    "业务权限存在持久失败关闭标记；拒绝恢复旧许可，需核对原始证据与账本".into(),
                );
            }
        }
        let state = match std::fs::File::open(&path) {
            Ok(file) => {
                if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_BYTES as u64 {
                    return Err("业务授权账本超过上限或不是文件；保留原文件".into());
                }
                let mut bytes = Vec::new();
                file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > MAX_BYTES {
                    return Err("业务授权账本超过上限；保留原文件".into());
                }
                let state: Ledger = serde_json::from_slice(&bytes)
                    .map_err(|_| "业务授权账本损坏；保留原文件，拒绝启动")?;
                if !state.valid() {
                    return Err("业务授权账本结构无效；保留原文件，拒绝启动".into());
                }
                state
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(initialized) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
                    _ => return Err("业务授权账本已初始化但文件缺失；拒绝恢复默认许可".into()),
                }
            }
            Err(_) => return Err("业务授权账本无法读取；拒绝启动".into()),
        };
        Ok(Self {
            path: Some(path),
            state: Mutex::new(state),
            failed: AtomicBool::new(false),
            plan_scopes: Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self {
            path: None,
            state: Mutex::new(Ledger::default()),
            failed: AtomicBool::new(false),
            plan_scopes: Mutex::new(None),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Ledger>, String> {
        if self.failed.load(Ordering::Acquire) {
            return Err("业务授权提交状态无法确认，已失败关闭；重启核对原账本".into());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| "业务授权账本不可用，已失败关闭".to_string())?;
        if self.failed.load(Ordering::Acquire) {
            return Err("业务授权提交状态无法确认，已失败关闭；重启核对原账本".into());
        }
        Ok(state)
    }

    pub(crate) fn revision(&self) -> Result<u64, String> {
        Ok(self.lock()?.revision)
    }

    /// 自然计划恢复其实际确认时的可信范围；模型参数、其他动作或拒绝记录不能绑定后台授权。
    pub(crate) fn reviewed_plan_scope(
        &self,
        plan_id: &str,
    ) -> Result<Option<ContextScope>, String> {
        let state = self.lock()?;
        let mut cache = self.plan_scopes.lock().map_err(|_| "计划授权索引不可用")?;
        if let Some(index) = &*cache
            && index.revision == state.revision
        {
            return Ok(index.scopes.get(plan_id).cloned());
        }
        let mut scopes = BTreeMap::new();
        for record in state.audit.iter().rev().filter(|record| {
            record.action == "manage_plan"
                && record.outcome == "review_allowed"
                && record.verdict.as_ref().is_some_and(|v| {
                    v.permits() && v.authorization == eve_developer_api::ReviewAuthorization::High
                })
        }) {
            let target = record.target.as_deref().ok_or("计划审核记录缺少完整动作")?;
            let request: serde_json::Value =
                serde_json::from_str(target).map_err(|_| "计划审核动作损坏")?;
            let operation = request["arguments"][0].as_str().ok_or("计划审核参数缺失")?;
            let operation: serde_json::Value =
                serde_json::from_str(operation).map_err(|_| "计划审核参数损坏")?;
            if request["program"] == "eve.manage_plan"
                && operation["operation"]["arguments"]["action"] == "confirm"
            {
                let id = operation["operation"]["plan"]["id"]
                    .as_str()
                    .ok_or("计划审核缺少计划编号")?;
                if operation["trusted_scope"]
                    != serde_json::to_value(&record.scope).map_err(|_| "计划范围无法编码")?
                    || request["user_messages"]
                        != serde_json::to_value(&record.user_messages)
                            .map_err(|_| "计划原话无法编码")?
                    || operation["turn_id"]
                        != serde_json::to_value(record.turn_id).map_err(|_| "计划轮次无法编码")?
                {
                    return Err("计划审核范围不一致".into());
                }
                scopes
                    .entry(id.to_owned())
                    .or_insert_with(|| record.scope.clone());
            }
        }
        let result = scopes.get(plan_id).cloned();
        *cache = Some(PlanScopes {
            revision: state.revision,
            scopes,
        });
        Ok(result)
    }

    pub(crate) fn fail_closed(&self) {
        self.failed.store(true, Ordering::Release);
    }

    pub(crate) fn persist_failure(
        &self,
        scope: &ContextScope,
        message_id: &str,
        latest_body: &str,
    ) -> Result<(), String> {
        self.fail_closed();
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path.parent().ok_or("业务失败标记目录不存在")?;
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(parent)?;
            let mut marker = tempfile::NamedTempFile::new_in(parent)?;
            let data = serde_json::json!({"version":1,"state":"failed_closed","scope":scope,"message_id":message_id,
                "latest_original_body":latest_body,"earlier_originals_complete":false,
                "reason":"收到真实明确缩权，但完整最近六条原话缺失；未恢复旧授权，也未补造缺失证据"});
            marker.write_all(&serde_json::to_vec(&data)?)?;
            marker.as_file().sync_all()?;
            marker
                .persist(parent.join("business-permissions.failed"))
                .map_err(|error| error.error)?;
            sync_directory(parent)
        };
        write().map_err(|_| "业务失败关闭标记无法持久化；进程仍保持关闭，原目录不可用".into())
    }

    pub(crate) fn grant(&self, scope: &ContextScope) -> Result<Option<Grant>, String> {
        Ok(self.lock()?.grants.get(&key(scope)).cloned())
    }

    pub(crate) fn pending_stops(&self) -> Result<Vec<Grant>, String> {
        Ok(self
            .lock()?
            .grants
            .values()
            .filter(|grant| grant.cancellation_pending)
            .cloned()
            .collect())
    }

    pub(crate) fn permits(&self, scope: &ContextScope) -> bool {
        self.grant(scope)
            .is_ok_and(|grant| grant.is_none_or(|g| g.state == BusinessState::Unlocked))
    }

    pub(crate) fn role_revoked(&self, id: &str) -> bool {
        self.lock()
            .map_or(true, |state| state.revoked_administrators.contains(id))
    }

    pub(crate) fn mutate(&self, mutation: Mutation) -> Result<bool, String> {
        let mut current = self.lock()?;
        if current.revision != mutation.expected_revision {
            return Err("审核期间授权状态已变化；本次没有写入，请依据新原话重新核对".into());
        }
        if mutation.outcome == "committed"
            && matches!(mutation.action.as_str(), "revoke" | "emergency_stop")
            && mutation.turn_id.is_none()
            && mutation.target.as_ref().is_some_and(|target| {
                current.audit.iter().any(|event| {
                    event.scope == mutation.scope
                        && event.target.as_ref() == Some(target)
                        && event.action == mutation.action
                        && event.outcome == "committed"
                })
            })
        {
            return Ok(false);
        }
        if !valid_scope(&mutation.scope)
            || !valid_messages(&mutation.user_messages)
            || mutation.at_ms == 0
        {
            return Err("业务授权缺少可信范围或完整用户原话".into());
        }
        let evidence_digest = evidence(&mutation.scope, mutation.turn_id, &mutation.user_messages);
        if mutation.outcome == "committed"
            && matches!(mutation.action.as_str(), "unlock" | "revoke")
            && current
                .grants
                .get(&key(&mutation.scope))
                .is_some_and(|grant| grant.cancellation_pending)
        {
            return Err("急停取消尚未核对完成，不能解锁或降级为普通撤销".into());
        }
        if mutation.outcome == "committed"
            && mutation.action == "unlock"
            && current.grants.get(&key(&mutation.scope)).is_some_and(|g| {
                g.evidence_digest == evidence_digest && g.state == BusinessState::Unlocked
            })
        {
            return Ok(false);
        }
        // 旧授权原话不能在撤销后重新恢复权限；必须得到新的真实用户消息/轮次。
        if mutation.outcome == "committed"
            && mutation.action == "unlock"
            && current.audit.iter().any(|event| {
                event.scope == mutation.scope
                    && event.action == "unlock"
                    && event.turn_id == mutation.turn_id
                    && event.user_messages == mutation.user_messages
                    && event.outcome == "committed"
            })
        {
            return Err("该用户原话已用于旧授权，不能在撤销后重放解锁".into());
        }
        let mut next = current.clone();
        next.revision = next.revision.checked_add(1).ok_or("授权账本修订已满")?;
        if mutation.outcome == "committed" {
            match mutation.action.as_str() {
                "unlock" | "revoke" | "emergency_stop" => {
                    let previous = next.grants.get(&key(&mutation.scope));
                    let state = match mutation.action.as_str() {
                        "unlock" => BusinessState::Unlocked,
                        "revoke" => BusinessState::Revoked,
                        _ => BusinessState::EmergencyStopped,
                    };
                    let issued_at_ms = if state == BusinessState::Unlocked {
                        mutation.at_ms
                    } else {
                        previous.map_or(mutation.at_ms, |g| g.issued_at_ms)
                    };
                    next.grants.insert(
                        key(&mutation.scope),
                        Grant {
                            scope: mutation.scope.clone(),
                            state,
                            allowed_businesses: vec!["all_business".into()],
                            cancellation_pending: state == BusinessState::EmergencyStopped,
                            issued_by: mutation.actor.clone(),
                            issued_at_ms,
                            revoked_at_ms: (state != BusinessState::Unlocked)
                                .then_some(mutation.at_ms),
                            revision: next.revision,
                            evidence_digest,
                            evidence_turn_id: mutation.turn_id,
                            user_messages: mutation.user_messages.clone(),
                        },
                    );
                }
                "emergency_stop_settled" => {
                    let grant = next
                        .grants
                        .get_mut(&key(&mutation.scope))
                        .filter(|grant| grant.state == BusinessState::EmergencyStopped)
                        .ok_or("当前范围没有待核对急停")?;
                    grant.cancellation_pending = false;
                }
                "revoke_administrator" => {
                    let target = mutation
                        .target
                        .as_ref()
                        .filter(|id| valid_role_id(id))
                        .ok_or("管理员标识无效")?;
                    next.revoked_administrators.insert(target.clone());
                }
                _ => return Err("业务授权动作无效".into()),
            }
        }
        next.audit.push(Audit {
            scope: mutation.scope,
            actor: mutation.actor,
            action: mutation.action,
            target: mutation.target,
            at_ms: mutation.at_ms,
            turn_id: mutation.turn_id,
            user_messages: mutation.user_messages,
            verdict: mutation.verdict,
            outcome: mutation.outcome,
        });
        if !next.valid() {
            if matches!(
                mutation_action(&next),
                "revoke" | "emergency_stop" | "revoke_administrator"
            ) {
                self.failed.store(true, Ordering::Release);
            }
            return Err("业务授权或审计容量/结构无效，未清空旧记录".into());
        }
        let bytes = serde_json::to_vec(&next).map_err(|_| "授权账本无法编码")?;
        if bytes.len() > MAX_BYTES {
            if matches!(
                mutation_action(&next),
                "revoke" | "emergency_stop" | "revoke_administrator"
            ) {
                self.failed.store(true, Ordering::Release);
            }
            return Err("业务授权账本容量已满，未裁剪原话或审计记录".into());
        }
        if let Some(path) = &self.path {
            let persist = || -> std::io::Result<()> {
                let parent = path
                    .parent()
                    .ok_or_else(|| std::io::Error::other("授权目录缺失"))?;
                std::fs::create_dir_all(parent)?;
                let mut file = tempfile::NamedTempFile::new_in(parent)?;
                file.write_all(&bytes)?;
                file.as_file().sync_all()?;
                file.persist(path).map_err(|error| error.error)?;
                let marker = parent.join("business-permissions.initialized");
                match std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(marker)
                {
                    Ok(mut marker) => {
                        marker.write_all(b"1\n")?;
                        marker.sync_all()?;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
                sync_directory(parent)
            };
            if persist().is_err() {
                self.failed.store(true, Ordering::Release);
                return Err("业务授权提交无法确认，已失败关闭；保留原文件，重启核对".into());
            }
        }
        *current = next;
        Ok(true)
    }
}

fn mutation_action(ledger: &Ledger) -> &str {
    ledger
        .audit
        .last()
        .map_or("", |event| event.action.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(session: &str) -> ContextScope {
        ContextScope {
            session_id: session.into(),
            user_id: "owner".into(),
        }
    }
    fn mutation(store: &Store, action: &str, turn: u64) -> Mutation {
        Mutation {
            scope: scope("a"),
            actor: "legacy:owner".into(),
            action: action.into(),
            target: None,
            at_ms: 100 + turn,
            turn_id: Some(turn),
            user_messages: vec![format!("本轮 {turn} 明确要求 {action}")],
            verdict: None,
            outcome: "committed".into(),
            expected_revision: store.revision().unwrap(),
        }
    }

    #[test]
    fn scope_grants_revocations_and_original_evidence_survive_restart_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut unlock = mutation(&store, "unlock", 1);
        unlock.user_messages = (0..6)
            .map(|i| format!("第 {i} 条完整原话 {} 末尾", "中".repeat(2000)))
            .collect();
        let evidence = unlock.user_messages.clone();
        assert!(store.mutate(unlock).unwrap());
        assert_eq!(
            store.grant(&scope("a")).unwrap().unwrap().user_messages,
            evidence
        );
        assert!(store.permits(&scope("a")));
        assert!(store.permits(&scope("b")));
        assert!(store.mutate(mutation(&store, "revoke", 2)).unwrap());
        let restarted = Store::open(directory.path()).unwrap();
        assert!(!restarted.permits(&scope("a")));
        assert!(restarted.permits(&scope("b")));
        let mut replay = mutation(&restarted, "unlock", 1);
        replay.user_messages = evidence;
        assert!(restarted.mutate(replay).unwrap_err().contains("重放"));
        assert!(restarted.mutate(mutation(&restarted, "unlock", 3)).unwrap());
        assert!(restarted.permits(&scope("a")));
        let disk: Ledger = serde_json::from_slice(
            &std::fs::read(directory.path().join("business-permissions.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(disk.audit.len(), 3);
        assert_eq!(disk.audit[1].action, "revoke");
    }

    #[test]
    fn stale_review_cannot_override_revocation_and_bad_storage_closes_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let stale_unlock = mutation(&store, "unlock", 1);
        store.mutate(mutation(&store, "emergency_stop", 2)).unwrap();
        assert!(
            store
                .mutate(stale_unlock)
                .unwrap_err()
                .contains("状态已变化")
        );
        assert!(!store.permits(&scope("a")));
        store
            .mutate(mutation(&store, "emergency_stop_settled", 2))
            .unwrap();
        std::fs::remove_file(directory.path().join("business-permissions.json")).unwrap();
        std::fs::create_dir(directory.path().join("business-permissions.json")).unwrap();
        assert!(store.mutate(mutation(&store, "unlock", 3)).is_err());
        assert!(
            !store.permits(&scope("b")),
            "提交不确定后未触及范围也失败关闭"
        );
        assert!(Store::open(directory.path()).is_err());
    }

    #[test]
    fn corrupt_state_is_preserved_and_role_revocation_is_persistent() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let mut revoke = mutation(&store, "revoke_administrator", 1);
        revoke.target = Some("legacy:administrator".into());
        store.mutate(revoke).unwrap();
        assert!(
            Store::open(directory.path())
                .unwrap()
                .role_revoked("legacy:administrator")
        );
        let path = directory.path().join("business-permissions.json");
        std::fs::write(&path, b"broken original").unwrap();
        assert!(Store::open(directory.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"broken original");
    }

    #[test]
    fn audit_capacity_cannot_leave_runtime_enabled_after_failed_stop() {
        let store = Store::memory();
        store.mutate(mutation(&store, "unlock", 1)).unwrap();
        {
            let mut state = store.state.lock().unwrap();
            let audit = state.audit[0].clone();
            state.audit.resize(MAX_RECORDS, audit);
        }
        assert!(store.mutate(mutation(&store, "emergency_stop", 2)).is_err());
        assert!(!store.permits(&scope("a")));
        assert!(!store.permits(&scope("b")));
    }

    #[test]
    fn interrupted_stop_blocks_unlock_until_cancel_recovers_and_evidence_change_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.mutate(mutation(&store, "emergency_stop", 1)).unwrap();
        let restarted = Store::open(directory.path()).unwrap();
        assert_eq!(restarted.pending_stops().unwrap().len(), 1);
        assert!(restarted.mutate(mutation(&restarted, "unlock", 2)).is_err());
        assert!(restarted.mutate(mutation(&restarted, "revoke", 2)).is_err());
        let stale_settle = mutation(&restarted, "emergency_stop_settled", 2);
        restarted
            .mutate(mutation(&restarted, "emergency_stop", 3))
            .unwrap();
        assert!(
            restarted.mutate(stale_settle).is_err(),
            "旧取消回执不得了结新急停"
        );
        restarted
            .mutate(mutation(&restarted, "emergency_stop_settled", 4))
            .unwrap();
        restarted.mutate(mutation(&restarted, "unlock", 5)).unwrap();
        let path = directory.path().join("business-permissions.json");
        let mut ledger: Ledger = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ledger.grants.values_mut().next().unwrap().user_messages[0].push_str("伪造追加");
        let corrupted = serde_json::to_vec(&ledger).unwrap();
        std::fs::write(&path, &corrupted).unwrap();
        assert!(Store::open(directory.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), corrupted);
    }

    #[test]
    fn missing_initialized_ledger_does_not_restore_default_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.mutate(mutation(&store, "revoke", 1)).unwrap();
        std::fs::remove_file(directory.path().join("business-permissions.json")).unwrap();
        assert!(Store::open(directory.path()).is_err());
        assert!(
            directory
                .path()
                .join("business-permissions.initialized")
                .exists()
        );
    }
}
