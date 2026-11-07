//! 不请求模型的本地健康巡检；只通知新出现的问题，发送前持久化，未知不重发。
use eve_llm_api::ContextScope;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
    sync::Mutex,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub code: String,
    /// 固定类别和计数，不包含日志正文、凭据或其他会话内容。
    pub summary: String,
}
pub trait HealthSource: Send + Sync {
    fn inspect(&self) -> Result<Vec<Finding>, String>;

    /// 默认检查是完整观测；有进程内计数或缺失探针的实现必须显式保留未知。
    /// `recovered` 可与 findings 重叠：两次巡检间确有恢复，又出现了新的同类故障。
    fn observe(&self, _previous: &[Finding], _checked_at_ms: u64) -> Result<Observation, String> {
        Ok(Observation {
            findings: self.inspect()?,
            unknown: vec![],
            recovered: vec![],
        })
    }
}
#[derive(Clone, Debug, Default)]
pub struct Observation {
    pub findings: Vec<Finding>,
    /// 仅引用 previous 中的 code；没有观测不能生成新事件或假装恢复。
    pub unknown: Vec<String>,
    /// previous 中已被实际成功观测结束的 code。
    pub recovered: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    Pending,
    Sending,
    Sent,
    Failed,
    Unknown,
    /// 宿主已记录 Eve 选择不说；没有发送尝试，可以安全回收这条旧通知。
    Suppressed,
    /// 未发送事件的所有发现均已恢复或被后继事件取代；保留逐项原因，不伪造回执。
    Retired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionReason {
    Recovered {},
    /// 旧格式只在 code 离开 active 后建立后继；不推断中间是否发生了实际成功。
    Superseded {
        notice_id: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub code: String,
    pub at_ms: u64,
    pub reason: ResolutionReason,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub id: String,
    pub at_ms: u64,
    pub findings: Vec<Finding>,
    pub owner: Option<ContextScope>,
    pub delivery: Delivery,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolutions: Vec<Resolution>,
}
impl Notice {
    pub fn unresolved(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| !self.resolutions.iter().any(|r| r.code == f.code))
    }
}
/// 只有本次确认为异常的项可发起通知；未知与部分恢复仍作为事实保存。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeCandidate {
    pub id: String,
    pub at_ms: u64,
    pub findings: Vec<Finding>,
    pub unknown: Vec<Finding>,
    pub resolutions: Vec<Resolution>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    version: u32,
    next_id: u64,
    pub checked_at_ms: u64,
    pub active: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<Finding>,
    pub notices: Vec<Notice>,
}
impl Snapshot {
    fn candidate(&self, notice: &Notice) -> Option<NoticeCandidate> {
        if notice.delivery != Delivery::Pending {
            return None;
        }
        let findings: Vec<_> = notice
            .unresolved()
            .filter(|f| self.active.iter().any(|a| a.code == f.code))
            .cloned()
            .collect();
        (!findings.is_empty()).then(|| NoticeCandidate {
            id: notice.id.clone(),
            at_ms: notice.at_ms,
            findings,
            unknown: notice
                .unresolved()
                .filter(|f| self.unknown.iter().any(|u| u.code == f.code))
                .cloned()
                .collect(),
            resolutions: notice.resolutions.clone(),
        })
    }
    pub fn candidates(&self) -> Vec<NoticeCandidate> {
        self.notices
            .iter()
            .filter_map(|n| self.candidate(n))
            .collect()
    }
}
pub struct HealthWatch {
    path: PathBuf,
    state: Mutex<Snapshot>,
}
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_NOTICES: usize = 128;
fn validate_findings(findings: &[Finding]) -> bool {
    let mut seen = BTreeSet::new();
    findings.len() <= 64
        && findings.iter().all(|f| {
            !f.code.is_empty()
                && f.code.len() <= 96
                && f.code
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && !f.summary.trim().is_empty()
                && f.summary.len() <= 512
                && !f.summary.chars().any(char::is_control)
                && seen.insert(&f.code)
        })
}
fn notice_number(id: &str) -> Option<u64> {
    id.strip_prefix("health-").and_then(|v| v.parse().ok())
}
fn validate_resolutions(notice: &Notice, snapshot: &Snapshot) -> bool {
    let mut codes = BTreeSet::new();
    notice.resolutions.iter().all(|r| {
        r.at_ms != 0
            && codes.insert(&r.code)
            && notice.findings.iter().any(|f| f.code == r.code)
            && match &r.reason {
                ResolutionReason::Recovered {} => true,
                ResolutionReason::Superseded { notice_id } => {
                    notice_number(notice_id).is_some_and(|id| {
                        id > notice_number(&notice.id).unwrap_or(u64::MAX) && id < snapshot.next_id
                    }) && snapshot
                        .notices
                        .iter()
                        .find(|n| n.id == *notice_id)
                        .is_none_or(|n| n.findings.iter().any(|f| f.code == r.code))
                }
            }
    }) && ((notice.resolutions.len() == notice.findings.len())
        == (notice.delivery == Delivery::Retired))
}

/// 只修复旧格式中尚未认领的前任，不改 Sending/Unknown 或任何已完成回执。
fn retire_legacy_predecessors(state: &mut Snapshot) -> bool {
    if state.version >= 3 {
        return false;
    }
    let history = state.notices.clone();
    let mut changed = false;
    for notice in &mut state.notices {
        if notice.delivery != Delivery::Pending {
            continue;
        }
        for finding in &notice.findings {
            if let Some(next) = history
                .iter()
                .filter(|n| {
                    notice_number(&n.id) > notice_number(&notice.id)
                        && n.findings.iter().any(|f| f.code == finding.code)
                })
                .min_by_key(|n| notice_number(&n.id))
            {
                notice.resolutions.push(Resolution {
                    code: finding.code.clone(),
                    at_ms: next.at_ms,
                    reason: ResolutionReason::Superseded {
                        notice_id: next.id.clone(),
                    },
                });
                changed = true;
            }
        }
        if notice.resolutions.len() == notice.findings.len() {
            notice.delivery = Delivery::Retired;
        }
    }
    if changed {
        state.version = 3;
    }
    changed
}
impl HealthWatch {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let mut state: Snapshot = match std::fs::read(&path) {
            Ok(bytes) if bytes.len() <= MAX_BYTES => {
                let raw: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(|_| "健康账本损坏；保留原文件")?;
                if raw["version"].as_u64().is_some_and(|v| v < 3)
                    && (raw.get("unknown").is_some()
                        || raw["notices"].as_array().is_some_and(|notices| {
                            notices.iter().any(|n| n.get("resolutions").is_some())
                        }))
                {
                    return Err("旧健康账本包含新格式字段；保留原文件".into());
                }
                serde_json::from_slice(&bytes).map_err(|_| "健康账本损坏；保留原文件")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot {
                version: 3,
                next_id: 1,
                checked_at_ms: 0,
                active: vec![],
                unknown: vec![],
                notices: vec![],
            },
            _ => return Err("健康账本不可读或过大；保留原文件".into()),
        };
        let mut ids = BTreeSet::new();
        if !matches!(state.version, 1..=3)
            || state.next_id == 0
            || state.notices.len() > MAX_NOTICES
            || !validate_findings(&state.active)
            || !validate_findings(&state.unknown)
            || state
                .unknown
                .iter()
                .any(|u| state.active.iter().any(|a| a.code == u.code))
            || state.notices.iter().any(|n| {
                let id = notice_number(&n.id);
                id.is_none_or(|v| v == 0 || v >= state.next_id)
                    || !ids.insert(&n.id)
                    || n.at_ms == 0
                    || n.findings.is_empty()
                    || !validate_findings(&n.findings)
                    || (state.version == 1 && n.delivery == Delivery::Suppressed)
                    || (state.version < 3 && n.delivery == Delivery::Retired)
                    || !validate_resolutions(n, &state)
                    || (n.owner.is_some()
                        == matches!(n.delivery, Delivery::Pending | Delivery::Retired))
                    || n.owner
                        .as_ref()
                        .is_some_and(|s| s.session_id.is_empty() || s.user_id.is_empty())
            })
        {
            return Err("健康账本结构无效；保留原文件".into());
        }
        let mut changed = retire_legacy_predecessors(&mut state);
        let mut pending_codes = BTreeSet::new();
        if state.notices.iter().any(|n| {
            n.delivery == Delivery::Pending
                && n.unresolved().any(|f| !pending_codes.insert(&f.code))
        }) {
            return Err("健康账本存在重复未结束事件；保留原文件".into());
        }
        for notice in &mut state.notices {
            if notice.delivery == Delivery::Sending {
                notice.delivery = Delivery::Unknown;
                changed = true;
            }
        }
        let watch = Self {
            path,
            state: Mutex::new(state),
        };
        if changed {
            watch.write(&watch.snapshot()?)?;
        }
        Ok(watch)
    }
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        self.state
            .lock()
            .map(|s| s.clone())
            .map_err(|_| "健康账本不可用".into())
    }
    /// 调用者提供检查时钟与最短周期；读取失败不把已有异常假装清除。
    pub fn check(
        &self,
        source: &dyn HealthSource,
        now: u64,
        interval_ms: u64,
    ) -> Result<(), String> {
        let mut guard = self.state.lock().map_err(|_| "健康账本不可用")?;
        if now == 0
            || (guard.checked_at_ms != 0 && now.saturating_sub(guard.checked_at_ms) < interval_ms)
        {
            return Ok(());
        }
        let previous: Vec<_> = guard
            .active
            .iter()
            .chain(&guard.unknown)
            .chain(
                guard
                    .notices
                    .iter()
                    .filter(|n| n.delivery == Delivery::Pending)
                    .flat_map(Notice::unresolved),
            )
            .map(|f| (f.code.clone(), f.clone()))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        let observation = source.observe(&previous, guard.checked_at_ms)?;
        let old: BTreeSet<_> = previous.iter().map(|f| f.code.as_str()).collect();
        let current: BTreeSet<_> = observation
            .findings
            .iter()
            .map(|f| f.code.as_str())
            .collect();
        let unknown: BTreeSet<_> = observation.unknown.iter().map(String::as_str).collect();
        let recovered: BTreeSet<_> = observation.recovered.iter().map(String::as_str).collect();
        if !validate_findings(&observation.findings)
            || unknown.len() != observation.unknown.len()
            || recovered.len() != observation.recovered.len()
            || !unknown.is_subset(&old)
            || !recovered.is_subset(&old)
            || !unknown.is_disjoint(&current)
            || !unknown.is_disjoint(&recovered)
        {
            return Err("健康检查结果无效，旧记录保留".into());
        }
        let fresh: Vec<_> = observation
            .findings
            .iter()
            .filter(|f| !old.contains(f.code.as_str()) || recovered.contains(f.code.as_str()))
            .cloned()
            .collect();
        let mut next = guard.clone();
        next.version = 3;
        next.checked_at_ms = now;
        next.active = observation.findings.clone();
        next.unknown = previous
            .iter()
            .filter(|f| unknown.contains(f.code.as_str()))
            .cloned()
            .collect();
        if !validate_findings(&next.unknown) {
            return Err("健康未知项超过容量；旧记录保留".into());
        }
        for notice in &mut next.notices {
            if notice.delivery != Delivery::Pending {
                continue;
            }
            let resolved: Vec<_> = notice
                .unresolved()
                .filter(|f| {
                    recovered.contains(f.code.as_str())
                        || (!current.contains(f.code.as_str())
                            && !unknown.contains(f.code.as_str()))
                })
                .map(|f| Resolution {
                    code: f.code.clone(),
                    at_ms: now,
                    reason: ResolutionReason::Recovered {},
                })
                .collect();
            notice.resolutions.extend(resolved);
            if notice.unresolved().next().is_none() {
                notice.delivery = Delivery::Retired;
            }
        }
        if !fresh.is_empty() {
            if next.notices.len() == MAX_NOTICES {
                if let Some(i) = next.notices.iter().position(|n| {
                    matches!(
                        n.delivery,
                        Delivery::Sent
                            | Delivery::Failed
                            | Delivery::Suppressed
                            | Delivery::Retired
                    )
                }) {
                    next.notices.remove(i);
                } else {
                    return Err("健康通知记录已满；旧记录保留，未覆盖".into());
                }
            }
            let id = format!("health-{}", next.next_id);
            next.next_id = next.next_id.checked_add(1).ok_or("健康通知编号用尽")?;
            next.notices.push(Notice {
                id,
                at_ms: now,
                findings: fresh,
                owner: None,
                delivery: Delivery::Pending,
                resolutions: vec![],
            });
        }
        self.write(&next)?;
        *guard = next;
        Ok(())
    }
    pub fn claim(&self, owner: ContextScope) -> Result<Option<Notice>, String> {
        self.claim_inner(None, None, owner)
    }

    /// 只认领宿主已审视的那条事件；ID 或观察时间不符时不挑选其他通知。
    pub fn claim_id(
        &self,
        id: &str,
        expected_at_ms: u64,
        owner: ContextScope,
    ) -> Result<Option<Notice>, String> {
        if id.is_empty() || expected_at_ms == 0 {
            return Err("健康通知候选无效".into());
        }
        self.claim_inner(Some((id, expected_at_ms)), None, owner)
    }

    /// 在同一账本锁内核对已审视的事实；部分恢复或未知变化后旧正文不能占用新事实。
    pub fn claim_candidate(
        &self,
        candidate: &NoticeCandidate,
        owner: ContextScope,
    ) -> Result<Option<Notice>, String> {
        self.claim_inner(
            Some((&candidate.id, candidate.at_ms)),
            Some(candidate),
            owner,
        )
    }

    fn claim_inner(
        &self,
        expected: Option<(&str, u64)>,
        candidate: Option<&NoticeCandidate>,
        owner: ContextScope,
    ) -> Result<Option<Notice>, String> {
        if owner.session_id.is_empty() || owner.user_id.is_empty() {
            return Err("健康通知缺少可信身份".into());
        }
        let mut guard = self.state.lock().map_err(|_| "健康账本不可用")?;
        let Some(index) = guard.notices.iter().position(|n| {
            expected.is_none_or(|(id, at_ms)| n.id == id && n.at_ms == at_ms)
                && guard
                    .candidate(n)
                    .is_some_and(|current| candidate.is_none_or(|c| *c == current))
        }) else {
            return Ok(None);
        };
        let mut next = guard.clone();
        let n = &mut next.notices[index];
        n.owner = Some(owner);
        n.delivery = Delivery::Sending;
        let claimed = n.clone();
        self.write(&next)?;
        *guard = next;
        Ok(Some(claimed))
    }

    /// 只结束指定的未发送通知，不制造平台回执；同一身份重复确认可幂等。
    pub fn suppress(
        &self,
        id: &str,
        expected_at_ms: u64,
        owner: ContextScope,
    ) -> Result<(), String> {
        self.suppress_inner(id, expected_at_ms, None, owner)
    }

    pub fn suppress_candidate(
        &self,
        candidate: &NoticeCandidate,
        owner: ContextScope,
    ) -> Result<(), String> {
        self.suppress_inner(&candidate.id, candidate.at_ms, Some(candidate), owner)
    }

    fn suppress_inner(
        &self,
        id: &str,
        expected_at_ms: u64,
        candidate: Option<&NoticeCandidate>,
        owner: ContextScope,
    ) -> Result<(), String> {
        if id.is_empty()
            || expected_at_ms == 0
            || owner.session_id.is_empty()
            || owner.user_id.is_empty()
        {
            return Err("健康通知决定缺少可信事件或身份".into());
        }
        let mut guard = self.state.lock().map_err(|_| "健康账本不可用")?;
        let mut next = guard.clone();
        let notice = next
            .notices
            .iter_mut()
            .find(|notice| notice.id == id && notice.at_ms == expected_at_ms)
            .ok_or("健康通知候选已变化")?;
        if notice.delivery == Delivery::Suppressed && notice.owner.as_ref() == Some(&owner) {
            return Ok(());
        }
        if notice.delivery != Delivery::Pending {
            return Err("健康通知已认领或结算，不能改为安静".into());
        }
        if guard
            .candidate(notice)
            .is_none_or(|current| candidate.is_some_and(|c| *c != current))
        {
            return Err("健康通知候选已变化".into());
        }
        notice.owner = Some(owner);
        notice.delivery = Delivery::Suppressed;
        next.version = next.version.max(2);
        self.write(&next)?;
        *guard = next;
        Ok(())
    }
    pub fn delivered(&self, id: &str, outcome: Delivery) -> Result<(), String> {
        if !matches!(
            outcome,
            Delivery::Sent | Delivery::Failed | Delivery::Unknown
        ) {
            return Err("健康通知回执无效".into());
        }
        let mut guard = self.state.lock().map_err(|_| "健康账本不可用")?;
        let mut next = guard.clone();
        let notice = next
            .notices
            .iter_mut()
            .find(|n| n.id == id && n.delivery == Delivery::Sending)
            .ok_or("健康通知不在发送中")?;
        notice.delivery = outcome;
        self.write(&next)?;
        *guard = next;
        Ok(())
    }
    fn write(&self, value: &Snapshot) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|_| "健康账本编码失败")?;
        if bytes.len() > MAX_BYTES {
            return Err("健康账本超过容量；原文件保留".into());
        }
        let parent = self.path.parent().ok_or("健康账本路径无效")?;
        std::fs::create_dir_all(parent).map_err(|_| "健康账本目录不可写")?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| "健康临时账本不可写")?;
        file.write_all(&bytes).map_err(|_| "健康账本写入失败")?;
        file.as_file().sync_all().map_err(|_| "健康账本同步失败")?;
        file.persist(&self.path)
            .map_err(|_| "健康账本提交失败；旧记录保留")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Source(bool);
    impl HealthSource for Source {
        fn inspect(&self) -> Result<Vec<Finding>, String> {
            Ok(if self.0 {
                vec![Finding {
                    code: "model.dialogue".into(),
                    summary: "对话连续失败 3 次".into(),
                }]
            } else {
                vec![]
            })
        }
    }
    fn owner() -> ContextScope {
        ContextScope {
            session_id: "session".into(),
            user_id: "owner".into(),
        }
    }
    fn finding(code: &str) -> Finding {
        Finding {
            code: code.into(),
            summary: format!("{code} 异常"),
        }
    }
    struct Observed(Observation);
    impl HealthSource for Observed {
        fn inspect(&self) -> Result<Vec<Finding>, String> {
            Ok(self.0.findings.clone())
        }
        fn observe(&self, _: &[Finding], _: u64) -> Result<Observation, String> {
            Ok(self.0.clone())
        }
    }
    fn observed(active: &[&str], unknown: &[&str], recovered: &[&str]) -> Observed {
        Observed(Observation {
            findings: active.iter().map(|code| finding(code)).collect(),
            unknown: unknown.iter().map(|code| (*code).into()).collect(),
            recovered: recovered.iter().map(|code| (*code).into()).collect(),
        })
    }

    #[test]
    fn exact_claim_never_substitutes_the_first_pending_incident() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        watch.check(&Source(false), 101, 1).unwrap();
        watch.check(&Source(true), 102, 1).unwrap();
        watch
            .check(&observed(&["model.dialogue", "disk.low"], &[], &[]), 103, 1)
            .unwrap();
        let notices = watch.snapshot().unwrap().notices;
        let retired = &notices[0];
        let first = &notices[1];
        let selected = &notices[2];
        let before = std::fs::read(&path).unwrap();
        assert!(
            watch
                .claim_id("missing", selected.at_ms, owner())
                .unwrap()
                .is_none()
        );
        assert!(
            watch
                .claim_id(&selected.id, selected.at_ms + 1, owner())
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(
            watch
                .claim_id(&retired.id, retired.at_ms, owner())
                .unwrap()
                .is_none()
        );
        let claimed = watch
            .claim_id(&selected.id, selected.at_ms, owner())
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, selected.id);
        assert_eq!(claimed.delivery, Delivery::Sending);
        assert_eq!(
            watch.snapshot().unwrap().notices[1].delivery,
            Delivery::Pending
        );
        assert!(
            watch
                .suppress(&selected.id, selected.at_ms, owner())
                .is_err()
        );
        assert!(
            watch
                .claim_id(&selected.id, selected.at_ms, owner())
                .unwrap()
                .is_none()
        );
        watch.delivered(&selected.id, Delivery::Unknown).unwrap();
        assert!(
            watch
                .suppress(&selected.id, selected.at_ms, owner())
                .is_err()
        );
        assert_eq!(watch.claim(owner()).unwrap().unwrap().id, first.id);
    }

    #[test]
    fn suppression_has_no_send_receipt_and_survives_restart_and_recovery_of_new_incidents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        let candidate = watch.snapshot().unwrap().notices[0].clone();
        let before = std::fs::read(&path).unwrap();
        assert!(watch.suppress(&candidate.id, 99, owner()).is_err());
        assert!(
            watch
                .delivered(&candidate.id, Delivery::Suppressed)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        watch
            .suppress(&candidate.id, candidate.at_ms, owner())
            .unwrap();
        let committed = std::fs::read(&path).unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&committed).unwrap();
        assert_eq!(wire["version"], 3);
        watch
            .suppress(&candidate.id, candidate.at_ms, owner())
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), committed);
        let watch = HealthWatch::open(path.clone()).unwrap();
        let suppressed = &watch.snapshot().unwrap().notices[0];
        assert_eq!(suppressed.delivery, Delivery::Suppressed);
        assert_eq!(suppressed.owner, Some(owner()));
        assert_eq!(std::fs::read(&path).unwrap(), committed);
        let other_owner = ContextScope {
            session_id: "other-session".into(),
            ..owner()
        };
        assert!(
            watch
                .suppress(&candidate.id, candidate.at_ms, other_owner)
                .is_err()
        );
        assert!(watch.delivered(&candidate.id, Delivery::Sent).is_err());
        assert!(
            watch
                .claim_id(&candidate.id, candidate.at_ms, owner())
                .unwrap()
                .is_none()
        );
        watch.check(&Source(true), 101, 1).unwrap();
        assert!(watch.claim(owner()).unwrap().is_none());
        watch.check(&Source(false), 102, 1).unwrap();
        watch.check(&Source(true), 103, 1).unwrap();
        let new = watch.claim(owner()).unwrap().unwrap();
        assert_ne!(new.id, candidate.id);
    }

    #[test]
    fn suppression_releases_capacity_without_reclaiming_unknown_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        let uncertain = watch.claim(owner()).unwrap().unwrap();
        watch.delivered(&uncertain.id, Delivery::Unknown).unwrap();
        for i in 1..=MAX_NOTICES + 1 {
            let at = 100 + 2 * i as u64;
            watch.check(&Source(false), at, 1).unwrap();
            watch.check(&Source(true), at + 1, 1).unwrap();
            let candidate = watch.snapshot().unwrap().notices.last().unwrap().clone();
            watch
                .suppress(&candidate.id, candidate.at_ms, owner())
                .unwrap();
        }
        let watch = HealthWatch::open(path).unwrap();
        let notices = watch.snapshot().unwrap().notices;
        assert_eq!(notices.len(), MAX_NOTICES);
        assert_eq!(notices[0].id, uncertain.id);
        assert_eq!(notices[0].delivery, Delivery::Unknown);
        assert!(
            notices
                .iter()
                .skip(1)
                .all(|notice| notice.delivery == Delivery::Suppressed)
        );
        assert!(watch.claim(owner()).unwrap().is_none());
    }

    #[test]
    fn failed_suppression_commit_leaves_candidate_pending_and_unknown_format_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        let candidate = watch.snapshot().unwrap().notices[0].clone();
        let pending = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            watch
                .suppress(&candidate.id, candidate.at_ms, owner())
                .is_err()
        );
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Pending
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, pending).unwrap();
        watch
            .suppress(&candidate.id, candidate.at_ms, owner())
            .unwrap();
        let mut invalid: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        invalid["version"] = 1.into();
        let bytes = serde_json::to_vec(&invalid).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(HealthWatch::open(path.clone()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn persistent_dedup_unknown_delivery_and_recovered_incident() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 30).unwrap();
        let n = watch.claim(owner()).unwrap().unwrap();
        assert!(watch.claim(owner()).unwrap().is_none());
        let watch = HealthWatch::open(path).unwrap();
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Unknown
        );
        watch.check(&Source(true), 150, 30).unwrap();
        assert!(watch.claim(owner()).unwrap().is_none());
        watch.check(&Source(false), 200, 30).unwrap();
        watch.check(&Source(true), 250, 30).unwrap();
        assert_ne!(watch.claim(owner()).unwrap().unwrap().id, n.id);
    }
    #[test]
    fn failed_storage_keeps_old_state_and_corruption_is_not_erased() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 30).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(watch.claim(owner()).is_err());
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Pending
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, "broken").unwrap();
        assert!(HealthWatch::open(path.clone()).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "broken");
    }

    #[test]
    fn recovery_retires_unsent_incident_and_recurrence_gets_a_new_opportunity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        let original = watch.snapshot().unwrap().candidates().remove(0);
        watch.check(&Source(false), 101, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert!(snapshot.candidates().is_empty());
        let notice = &snapshot.notices[0];
        assert_eq!(notice.delivery, Delivery::Retired);
        assert!(notice.owner.is_none());
        assert_eq!(notice.findings, original.findings);
        assert_eq!(notice.resolutions[0].reason, ResolutionReason::Recovered {});
        assert_eq!(notice.resolutions[0].at_ms, 101);
        assert!(watch.claim_candidate(&original, owner()).unwrap().is_none());
        assert!(watch.delivered(&original.id, Delivery::Sent).is_err());
        let bytes = std::fs::read(&path).unwrap();
        let watch = HealthWatch::open(path.clone()).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
        assert!(watch.claim(owner()).unwrap().is_none());
        watch.check(&Source(true), 102, 1).unwrap();
        let candidates = watch.snapshot().unwrap().candidates();
        assert_eq!(candidates.len(), 1);
        assert_ne!(candidates[0].id, original.id);
        assert_eq!(watch.snapshot().unwrap().notices[0], *notice);
    }

    #[test]
    fn partial_recovery_never_revives_the_old_code_and_rejects_stale_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch
            .check(&observed(&["a", "b"], &[], &[]), 100, 1)
            .unwrap();
        let original = watch.snapshot().unwrap().candidates().remove(0);
        watch
            .check(&observed(&["a", "b"], &[], &[]), 101, 1)
            .unwrap();
        assert_eq!(
            watch.snapshot().unwrap().candidates(),
            vec![original.clone()]
        );
        watch.check(&observed(&["b"], &[], &[]), 102, 1).unwrap();
        let partial = watch.snapshot().unwrap().candidates().remove(0);
        assert_eq!(partial.findings, vec![finding("b")]);
        assert_eq!(partial.resolutions[0].code, "a");
        let before = std::fs::read(&path).unwrap();
        assert!(watch.claim_candidate(&original, owner()).unwrap().is_none());
        assert!(watch.suppress_candidate(&original, owner()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        watch
            .check(&observed(&["a", "b"], &[], &[]), 103, 1)
            .unwrap();
        let snapshot = watch.snapshot().unwrap();
        let candidates = snapshot.candidates();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0], partial);
        assert_eq!(candidates[1].findings, vec![finding("a")]);
        assert_ne!(candidates[1].id, original.id);
        assert_eq!(snapshot.notices[0].findings, original.findings);
        watch.claim_candidate(&partial, owner()).unwrap().unwrap();
        watch.check(&observed(&[], &[], &[]), 104, 1).unwrap();
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Sending
        );
        assert_eq!(
            watch.snapshot().unwrap().notices[0].resolutions,
            partial.resolutions
        );
        let watch = HealthWatch::open(path).unwrap();
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Unknown
        );
        assert_eq!(
            watch.snapshot().unwrap().notices[1].delivery,
            Delivery::Retired
        );
    }

    #[test]
    fn unobserved_incident_survives_restart_without_a_new_incident_or_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch
            .check(&observed(&["a", "b"], &[], &[]), 100, 1)
            .unwrap();
        let original = watch.snapshot().unwrap().candidates().remove(0);
        watch.check(&observed(&["b"], &["a"], &[]), 101, 1).unwrap();
        let partial = watch.snapshot().unwrap().candidates().remove(0);
        assert_eq!(partial.findings, vec![finding("b")]);
        assert_eq!(partial.unknown, vec![finding("a")]);
        assert!(partial.resolutions.is_empty());
        assert!(watch.claim_candidate(&original, owner()).unwrap().is_none());
        watch
            .check(&observed(&[], &["a", "b"], &[]), 102, 1)
            .unwrap();
        let watch = HealthWatch::open(path).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert!(snapshot.candidates().is_empty());
        assert!(snapshot.active.is_empty());
        assert_eq!(snapshot.unknown, vec![finding("a"), finding("b")]);
        assert_eq!(snapshot.notices.len(), 1);
        assert_eq!(snapshot.notices[0].delivery, Delivery::Pending);
        assert!(
            watch
                .claim_id(&original.id, original.at_ms, owner())
                .unwrap()
                .is_none()
        );
        assert!(
            watch
                .suppress(&original.id, original.at_ms, owner())
                .is_err()
        );
        watch
            .check(&observed(&["a", "b"], &[], &[]), 103, 1)
            .unwrap();
        assert_eq!(watch.snapshot().unwrap().candidates(), vec![original]);
        watch
            .check(&observed(&[], &[], &["a", "b"]), 104, 1)
            .unwrap();
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Retired
        );
    }

    #[test]
    fn successful_observation_between_checks_separates_same_code_incidents() {
        let dir = tempfile::tempdir().unwrap();
        let watch = HealthWatch::open(dir.path().join("health.json")).unwrap();
        watch.check(&observed(&["a"], &[], &[]), 100, 1).unwrap();
        let original = watch.snapshot().unwrap().candidates().remove(0);
        watch.check(&observed(&["a"], &[], &["a"]), 101, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert_eq!(snapshot.notices.len(), 2);
        assert_eq!(snapshot.notices[0].delivery, Delivery::Retired);
        assert_eq!(
            snapshot.notices[0].resolutions[0].reason,
            ResolutionReason::Recovered {}
        );
        assert_eq!(snapshot.candidates().len(), 1);
        assert_ne!(snapshot.candidates()[0].id, original.id);
    }

    #[test]
    fn legacy_predecessors_retire_without_inventing_recovery_or_touching_receipts() {
        for version in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("health.json");
            let mut notices = vec![];
            for (id, delivery, codes) in [
                (1, Delivery::Pending, vec!["a", "b"]),
                (2, Delivery::Pending, vec!["a"]),
                (3, Delivery::Unknown, vec!["x"]),
                (4, Delivery::Sending, vec!["y"]),
                (5, Delivery::Pending, vec!["a", "x", "y"]),
            ] {
                let has_owner = delivery != Delivery::Pending;
                notices.push(Notice {
                    id: format!("health-{id}"),
                    at_ms: 100 + id,
                    findings: codes.into_iter().map(finding).collect(),
                    owner: has_owner.then(owner),
                    delivery,
                    resolutions: vec![],
                });
            }
            let legacy = Snapshot {
                version,
                next_id: 6,
                checked_at_ms: 110,
                active: vec![finding("a"), finding("b"), finding("x"), finding("y")],
                unknown: vec![],
                notices,
            };
            std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
            let watch = HealthWatch::open(path.clone()).unwrap();
            let snapshot = watch.snapshot().unwrap();
            assert_eq!(snapshot.version, 3);
            assert_eq!(snapshot.notices[0].delivery, Delivery::Pending);
            assert_eq!(
                snapshot.notices[0].resolutions[0].reason,
                ResolutionReason::Superseded {
                    notice_id: "health-2".into()
                }
            );
            assert_eq!(snapshot.notices[1].delivery, Delivery::Retired);
            assert!(snapshot.notices[1].owner.is_none());
            assert_eq!(
                snapshot.notices[1].resolutions[0].reason,
                ResolutionReason::Superseded {
                    notice_id: "health-5".into()
                }
            );
            assert_eq!(snapshot.notices[2], legacy.notices[2]);
            assert_eq!(snapshot.notices[3].delivery, Delivery::Unknown);
            assert!(snapshot.notices[3].resolutions.is_empty());
            let candidates = snapshot.candidates();
            assert_eq!(candidates.len(), 2);
            assert_eq!(candidates[0].findings, vec![finding("b")]);
            assert_eq!(candidates[1].id, "health-5");
            let committed = std::fs::read(&path).unwrap();
            HealthWatch::open(path.clone()).unwrap();
            assert_eq!(std::fs::read(path).unwrap(), committed);
        }
    }

    #[test]
    fn invalid_new_schema_is_preserved_before_any_sending_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&observed(&["a"], &[], &[]), 100, 1).unwrap();
        watch.check(&observed(&[], &[], &[]), 101, 1).unwrap();
        watch.check(&observed(&["b"], &[], &[]), 102, 1).unwrap();
        watch.claim(owner()).unwrap().unwrap();
        let valid: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut cases = vec![];
        let mut bad = valid.clone();
        bad["version"] = 2.into();
        cases.push(bad);
        let mut bad = valid.clone();
        bad["notices"][0]["resolutions"][0]["code"] = "other".into();
        cases.push(bad);
        let mut bad = valid.clone();
        bad["notices"][0]["resolutions"][0]["at_ms"] = 0.into();
        cases.push(bad);
        let mut bad = valid.clone();
        bad["notices"][0]["resolutions"][0]["reason"] =
            serde_json::json!({"kind":"superseded","notice_id":"health-1"});
        cases.push(bad);
        let mut bad = valid.clone();
        bad["notices"][0]["owner"] = serde_json::to_value(owner()).unwrap();
        cases.push(bad);
        let mut bad = valid.clone();
        bad["notices"][0]["delivery"] = "Pending".into();
        cases.push(bad);
        let mut bad = valid.clone();
        bad["unknown"] = valid["active"].clone();
        cases.push(bad);
        let mut bad = valid.clone();
        bad["notices"][0]["resolutions"][0]["reason"]["invented"] = true.into();
        cases.push(bad);
        for bad in cases {
            let bytes = serde_json::to_vec(&bad).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(HealthWatch::open(path.clone()).is_err(), "accepted {bad}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        for version in [1, 2] {
            let mut raw = serde_json::json!({"version":version,"next_id":1,
                "checked_at_ms":0,"active":[],"notices":[],"unknown":[]});
            let bytes = serde_json::to_vec(&raw).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(HealthWatch::open(path.clone()).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            raw.as_object_mut().unwrap().remove("unknown");
            let bytes = serde_json::to_vec(&raw).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            HealthWatch::open(path.clone()).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn failed_or_invalid_observation_commit_keeps_the_pending_incident() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&observed(&["a"], &[], &[]), 100, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        for source in [
            observed(&[], &["not-seen"], &[]),
            observed(&["a"], &["a"], &[]),
            observed(&[], &["a"], &["a"]),
            observed(&[], &[], &["not-seen"]),
        ] {
            assert!(watch.check(&source, 101, 1).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(watch.check(&observed(&[], &[], &[]), 101, 1).is_err());
        assert_eq!(watch.snapshot().unwrap().notices, snapshot.notices);
        assert_eq!(watch.snapshot().unwrap().checked_at_ms, 100);
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, bytes).unwrap();
        watch.check(&observed(&[], &[], &[]), 101, 1).unwrap();
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
            Delivery::Retired
        );
    }

    #[test]
    fn retired_history_releases_capacity_without_discarding_unknown_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        let uncertain = watch.claim(owner()).unwrap().unwrap();
        watch.delivered(&uncertain.id, Delivery::Unknown).unwrap();
        for i in 1..=MAX_NOTICES + 1 {
            let at = 100 + 2 * i as u64;
            watch.check(&Source(false), at, 1).unwrap();
            watch.check(&Source(true), at + 1, 1).unwrap();
        }
        let watch = HealthWatch::open(path).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert_eq!(snapshot.notices.len(), MAX_NOTICES);
        assert_eq!(snapshot.notices[0].id, uncertain.id);
        assert_eq!(snapshot.notices[0].delivery, Delivery::Unknown);
        assert!(
            snapshot.notices[1..MAX_NOTICES - 1]
                .iter()
                .all(|n| n.delivery == Delivery::Retired)
        );
        assert_eq!(snapshot.candidates().len(), 1);
    }

    #[test]
    fn full_ledger_preserves_pending_and_unknown_until_recovery_frees_a_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let notices = (1..=MAX_NOTICES)
            .map(|id| Notice {
                id: format!("health-{id}"),
                at_ms: id as u64,
                findings: vec![finding(if id == 1 { "pending" } else { "uncertain" })],
                owner: (id != 1).then(owner),
                delivery: if id == 1 {
                    Delivery::Pending
                } else {
                    Delivery::Unknown
                },
                resolutions: vec![],
            })
            .collect();
        let snapshot = Snapshot {
            version: 3,
            next_id: MAX_NOTICES as u64 + 1,
            checked_at_ms: 200,
            active: vec![finding("pending")],
            unknown: vec![],
            notices,
        };
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let watch = HealthWatch::open(path.clone()).unwrap();
        assert!(
            watch
                .check(&observed(&["pending", "new"], &[], &[]), 201, 1)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(watch.snapshot().unwrap().notices, snapshot.notices);
        watch.check(&observed(&["new"], &[], &[]), 202, 1).unwrap();
        let snapshot = watch.snapshot().unwrap();
        assert_eq!(snapshot.notices.len(), MAX_NOTICES);
        assert!(
            snapshot.notices[..MAX_NOTICES - 1]
                .iter()
                .all(|n| n.delivery == Delivery::Unknown)
        );
        assert_eq!(snapshot.candidates().len(), 1);
        assert_eq!(snapshot.candidates()[0].findings, vec![finding("new")]);
    }
}
