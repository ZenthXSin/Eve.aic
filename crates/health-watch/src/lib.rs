//! 不请求模型的本地健康巡检；只通知新出现的问题，发送前持久化，未知不重发。
use eve_llm_api::ContextScope;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, io::Write, path::PathBuf, sync::Mutex};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub code: String,
    /// 固定类别和计数，不包含日志正文、凭据或其他会话内容。
    pub summary: String,
}
pub trait HealthSource: Send + Sync {
    fn inspect(&self) -> Result<Vec<Finding>, String>;
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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notice {
    pub id: String,
    pub at_ms: u64,
    pub findings: Vec<Finding>,
    pub owner: Option<ContextScope>,
    pub delivery: Delivery,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    version: u32,
    next_id: u64,
    pub checked_at_ms: u64,
    pub active: Vec<Finding>,
    pub notices: Vec<Notice>,
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
impl HealthWatch {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        let mut state: Snapshot = match std::fs::read(&path) {
            Ok(bytes) if bytes.len() <= MAX_BYTES => {
                serde_json::from_slice(&bytes).map_err(|_| "健康账本损坏；保留原文件")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Snapshot {
                version: 1,
                next_id: 1,
                checked_at_ms: 0,
                active: vec![],
                notices: vec![],
            },
            _ => return Err("健康账本不可读或过大；保留原文件".into()),
        };
        let mut ids = BTreeSet::new();
        if !matches!(state.version, 1 | 2)
            || state.next_id == 0
            || state.notices.len() > MAX_NOTICES
            || !validate_findings(&state.active)
            || state.notices.iter().any(|n| {
                let id =
                    n.id.strip_prefix("health-")
                        .and_then(|v| v.parse::<u64>().ok());
                id.is_none_or(|v| v == 0 || v >= state.next_id)
                    || !ids.insert(&n.id)
                    || n.at_ms == 0
                    || n.findings.is_empty()
                    || !validate_findings(&n.findings)
                    || (state.version == 1 && n.delivery == Delivery::Suppressed)
                    || (n.owner.is_some() != (n.delivery != Delivery::Pending))
                    || n.owner
                        .as_ref()
                        .is_some_and(|s| s.session_id.is_empty() || s.user_id.is_empty())
            })
        {
            return Err("健康账本结构无效；保留原文件".into());
        }
        let mut changed = false;
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
        let findings = source.inspect()?;
        if !validate_findings(&findings) {
            return Err("健康检查结果无效，旧记录保留".into());
        }
        let old: BTreeSet<_> = guard.active.iter().map(|f| f.code.as_str()).collect();
        let fresh: Vec<_> = findings
            .iter()
            .filter(|f| !old.contains(f.code.as_str()))
            .cloned()
            .collect();
        let mut next = guard.clone();
        next.checked_at_ms = now;
        next.active = findings;
        if !fresh.is_empty() {
            if next.notices.len() == MAX_NOTICES {
                if let Some(i) = next.notices.iter().position(|n| {
                    matches!(
                        n.delivery,
                        Delivery::Sent | Delivery::Failed | Delivery::Suppressed
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
            });
        }
        self.write(&next)?;
        *guard = next;
        Ok(())
    }
    pub fn claim(&self, owner: ContextScope) -> Result<Option<Notice>, String> {
        self.claim_inner(None, owner)
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
        self.claim_inner(Some((id, expected_at_ms)), owner)
    }

    fn claim_inner(
        &self,
        expected: Option<(&str, u64)>,
        owner: ContextScope,
    ) -> Result<Option<Notice>, String> {
        if owner.session_id.is_empty() || owner.user_id.is_empty() {
            return Err("健康通知缺少可信身份".into());
        }
        let mut guard = self.state.lock().map_err(|_| "健康账本不可用")?;
        let mut next = guard.clone();
        let Some(n) = next.notices.iter_mut().find(|n| {
            n.delivery == Delivery::Pending
                && expected.is_none_or(|(id, at_ms)| n.id == id && n.at_ms == at_ms)
        }) else {
            return Ok(None);
        };
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
        notice.owner = Some(owner);
        notice.delivery = Delivery::Suppressed;
        next.version = 2;
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
    #[test]
    fn exact_claim_never_substitutes_the_first_pending_incident() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let watch = HealthWatch::open(path.clone()).unwrap();
        watch.check(&Source(true), 100, 1).unwrap();
        watch.check(&Source(false), 101, 1).unwrap();
        watch.check(&Source(true), 102, 1).unwrap();
        let notices = watch.snapshot().unwrap().notices;
        let first = &notices[0];
        let selected = &notices[1];
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
        let claimed = watch
            .claim_id(&selected.id, selected.at_ms, owner())
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, selected.id);
        assert_eq!(claimed.delivery, Delivery::Sending);
        assert_eq!(
            watch.snapshot().unwrap().notices[0].delivery,
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
        assert_eq!(wire["version"], 2);
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
}
