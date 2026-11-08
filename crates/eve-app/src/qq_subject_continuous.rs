//! 操作者部署授权只在启动时接入原主体服务，绝不伪造会话输入或改写账本文件。
use super::*;
use std::{io::Read, path::Path};

const ENVIRONMENT: &str = "EVE_HOST_AUTONOMY_CONFIG";

fn read_seed(path: &Path) -> Result<HostAutonomySeed, crate::AppError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > 6000 {
        return Err("宿主自主授权配置不是有效普通文件；未创建授权".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(6001)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 6000 {
        return Err("宿主自主授权配置超出容量；未裁剪".into());
    }
    let seed: HostAutonomySeed = serde_json::from_slice(&bytes)?;
    seed.validate()?;
    eve_task_api::validate_review_messages(&seed.review_messages)?;
    Ok(seed)
}

pub(crate) fn install_host_autonomy(
    subject: &Arc<dyn SubjectAdmin>,
    tasks: &Arc<dyn TaskAdmin>,
    owner: &Arc<dyn AutonomyOwner>,
    sessions: &Arc<dyn eve_session_api::SessionService>,
    now: u64,
) -> Result<(), crate::AppError> {
    let Some(path) = std::env::var_os(ENVIRONMENT) else {
        return Ok(());
    };
    let seed = read_seed(Path::new(&path))?;
    install_seed(subject, tasks, owner, sessions, &seed, now)?;
    Ok(())
}

fn install_seed(
    subject: &Arc<dyn SubjectAdmin>,
    tasks: &Arc<dyn TaskAdmin>,
    owner: &Arc<dyn AutonomyOwner>,
    sessions: &Arc<dyn eve_session_api::SessionService>,
    seed: &HostAutonomySeed,
    now: u64,
) -> SubjectResult<()> {
    seed.validate()?;
    let _mutation = MUTATIONS.lock().map_err(|_| SubjectError::Unavailable)?;
    let snapshot = subject.snapshot_for(&seed.scope)?;
    let marker = subject.autonomy_memory().ok_or(SubjectError::Unavailable)?;
    // 启动配置只负责首次接入；后来的暂停、撤销、到期或新授权一概不覆盖。
    if let Some(policy) = snapshot.foci.iter().find(|f| f.id == POLICY_ID) {
        let previous: Grant =
            serde_json::from_str(&policy.goal).map_err(|_| SubjectError::CorruptState)?;
        if !previous.valid() {
            return Err(SubjectError::CorruptState);
        }
        if !marker.host_authorization_seen(&seed.scope)? {
            marker.record_host_authorization(
                &seed.scope,
                &seed.authorization_id,
                &previous.evidence_digest,
                now,
            )?;
        }
        return Ok(());
    }
    // 防止用户取消的旧策略被关注容量回收后，配置再次把它当作从未启用。
    if marker.host_authorization_seen(&seed.scope)? {
        eprintln!(
            "EVE_HOST_AUTONOMY installed=false prior_receipt_without_policy=true grant_recreated=false"
        );
        return Ok(());
    }
    let identity = identity(&seed.scope);
    if scope(&identity) != seed.scope
        || !owner.allows_autonomy(&identity)
        || !AutonomyOwner::is_owner(owner.as_ref(), &identity)
    {
        return Err(SubjectError::InvalidInput);
    }
    let key =
        eve_session_api::SessionKey::new(identity.session_id.clone(), identity.user_id.clone())
            .map_err(|_| SubjectError::InvalidInput)?;
    let history = sessions
        .snapshot(&key)
        .map_err(|_| SubjectError::Unavailable)?
        .ok_or(SubjectError::NotFound)?;
    if history.key != key {
        return Err(SubjectError::CorruptState);
    }
    if history.turns.is_empty() {
        return Err(SubjectError::InvalidInput);
    }
    let tasks = tasks
        .scheduling_snapshot()
        .map_err(|_| SubjectError::Unavailable)?;
    tasks.validate().map_err(|_| SubjectError::CorruptState)?;
    if tasks.tasks.iter().any(|t| {
        t.owner.session_id == identity.session_id
            && t.owner.user_id == identity.user_id
            && t.source.starts_with(IDLE_TASK_SOURCE_PREFIX)
    }) {
        return Err(SubjectError::InvalidTransition);
    }
    let grant = Grant {
        version: 1,
        generation: 1,
        max_goals: seed.max_goals,
        issued: 0,
        steps_per_goal: seed.steps_per_goal,
        interests: seed.interests.clone(),
        evidence_digest: id(
            &seed.scope,
            &format!(
                "host:{}",
                serde_json::to_string(seed).map_err(|_| SubjectError::InvalidInput)?
            ),
        ),
        configured_turn_id: Some(history.turns.last().ok_or(SubjectError::InvalidInput)?.id),
        review_messages: seed.review_messages.clone(),
        blocked_task_id: None,
        expires_at_ms: u64::MAX,
        idle_reflection: true,
        window: Some(AutonomyWindow {
            started_at_ms: now,
            period_ms: AUTONOMY_DAY_MS,
            used: 0,
        }),
    };
    if !grant.valid() {
        return Err(SubjectError::InvalidInput);
    }
    let goal = serde_json::to_string(&grant).map_err(|_| SubjectError::InvalidInput)?;
    let policy = SubjectFocus {
        id: POLICY_ID.into(),
        scope: seed.scope.clone(),
        title: "宿主操作者已授权持续自主回顾与行动".into(),
        goal,
        task_id: None,
        status: FocusStatus::Active,
        next_check_at_ms: now,
        updated_at_ms: now,
    };
    validate_focus(&policy)?;
    // 先保存一次性接入记录，跨账本中断时宁可等待核对，不重复赋予预算。
    marker.record_host_authorization(
        &seed.scope,
        &seed.authorization_id,
        &grant.evidence_digest,
        now,
    )?;
    subject.upsert_focus(policy, now)?;
    eprintln!(
        "EVE_HOST_AUTONOMY installed=true period_hours=24 max_goals={} steps_per_goal={} raw_message_created=false",
        seed.max_goals, seed.steps_per_goal
    );
    Ok(())
}

/// 后台任务的滚动配额覆盖同一用户所有会话；达到上限时等待，不提前扣自主名额。
pub(super) fn task_admission_wait(
    tasks: &Arc<dyn TaskAdmin>,
    principal: &str,
    now: u64,
) -> SubjectResult<Option<u64>> {
    let state = tasks
        .scheduling_snapshot()
        .map_err(|_| SubjectError::Unavailable)?;
    state.validate().map_err(|_| SubjectError::CorruptState)?;
    let mine: Vec<_> = state
        .tasks
        .iter()
        .filter(|t| t.owner.user_id == principal)
        .collect();
    let mut next = 0;
    if mine.iter().filter(|t| !t.status.finished()).count() >= eve_task_api::MAX_ACTIVE_PER_OWNER {
        next = now
            .checked_add(AUTONOMY_INTERVAL_MS)
            .ok_or(SubjectError::InvalidInput)?;
    }
    let mut recent: Vec<_> = mine
        .iter()
        .filter(|t| now.saturating_sub(t.created_at_ms) < eve_task_api::DAY_MS)
        .map(|t| t.created_at_ms)
        .collect();
    recent.sort_unstable();
    if recent.len() >= eve_task_api::MAX_CREATED_PER_OWNER_PER_DAY {
        let first_available = recent[recent.len() - eve_task_api::MAX_CREATED_PER_OWNER_PER_DAY]
            .checked_add(eve_task_api::DAY_MS)
            .ok_or(SubjectError::InvalidInput)?;
        next = next.max(first_available);
    }
    Ok((next > 0).then_some(next))
}
