//! 会话分段设置契约：用户以明确命令选择是否分段、最多几段与停顿节奏。
//! 设置只能在宿主策略的上限内收窄或放宽，不能超过宿主允许的段数和停顿。
use crate::{
    FALLBACK_PLANNER, MAX_PAUSE_MS, MAX_SEGMENTS, SegmentError, SegmentLimits, SegmentPlan,
    SegmentPlanner, SegmentResult, plan_or_single,
};
use std::fmt;

/// 设置段数至少 2；需要整条发送时用 `enabled = false`，保持唯一表示。
pub const MIN_PREFERRED_SEGMENTS: usize = 2;
pub const DEFAULT_PAUSE_PERCENT: u16 = 100;
pub const MAX_PAUSE_PERCENT: u16 = 200;
const MAX_SCOPE_ID_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentPreferenceError {
    InvalidInput,
    LimitReached,
    Unavailable,
    Storage,
    CorruptState,
    UnsupportedVersion,
}
pub type SegmentPreferenceResult<T> = Result<T, SegmentPreferenceError>;
impl fmt::Display for SegmentPreferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidInput => "分段设置输入无效",
            Self::LimitReached => "分段设置容量已满；保留原设置",
            Self::Unavailable => "分段设置服务不可用",
            Self::Storage => "分段设置保存结果无法确认；须重新打开确认持久状态",
            Self::CorruptState => "分段设置状态损坏；未清空",
            Self::UnsupportedVersion => "分段设置状态版本不兼容；未清空",
        })
    }
}
impl std::error::Error for SegmentPreferenceError {}

/// 可信宿主绑定的范围；不从正文解析身份，不跨通道合并同名用户。
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct SegmentScope {
    pub channel: String,
    pub session_id: String,
    pub user_id: String,
}
impl SegmentScope {
    pub fn validate(&self) -> SegmentPreferenceResult<()> {
        let valid = |id: &String| {
            !id.is_empty()
                && id.len() <= MAX_SCOPE_ID_BYTES
                && id.trim() == id.as_str()
                && !id.chars().any(char::is_control)
        };
        if [&self.channel, &self.session_id, &self.user_id]
            .into_iter()
            .all(valid)
        {
            Ok(())
        } else {
            Err(SegmentPreferenceError::InvalidInput)
        }
    }
}
impl fmt::Debug for SegmentScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SegmentScope(<redacted>)")
    }
}

/// 用户的明确选择；`None` 表示跟随宿主默认。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SegmentPreference {
    pub enabled: Option<bool>,
    pub max_segments: Option<usize>,
    pub pause_percent: Option<u16>,
}
/// 一项明确修改；将来经用户明确接受的结构化建议也只能写这些值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentChange {
    Enabled(bool),
    MaxSegments(usize),
    PausePercent(u16),
    Reset,
}
impl SegmentPreference {
    pub fn validate(&self) -> SegmentPreferenceResult<()> {
        if self
            .max_segments
            .is_some_and(|n| !(MIN_PREFERRED_SEGMENTS..=MAX_SEGMENTS).contains(&n))
            || self.pause_percent.is_some_and(|p| p > MAX_PAUSE_PERCENT)
        {
            Err(SegmentPreferenceError::InvalidInput)
        } else {
            Ok(())
        }
    }
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
    /// 纯函数：越界时返回 InvalidInput，不部分应用。
    pub fn apply(self, change: SegmentChange) -> SegmentPreferenceResult<Self> {
        let next = match change {
            SegmentChange::Enabled(enabled) => Self {
                enabled: Some(enabled),
                ..self
            },
            SegmentChange::MaxSegments(n) => Self {
                max_segments: Some(n),
                ..self
            },
            SegmentChange::PausePercent(p) => Self {
                pause_percent: Some(p),
                ..self
            },
            SegmentChange::Reset => Self::default(),
        };
        next.validate()?;
        Ok(next)
    }
    /// 本轮实际方式：段数不超过宿主上限；停顿上限随百分比缩放并不超过宿主停顿上限。
    pub fn effective(&self, policy: &SegmentPolicy) -> SegmentResult<EffectiveSegmentation> {
        policy.validate()?;
        self.validate().map_err(|_| SegmentError::InvalidLimits)?;
        let pause_percent = self.pause_percent.unwrap_or(DEFAULT_PAUSE_PERCENT);
        let scaled = policy.defaults.max_pause_ms * u64::from(pause_percent) / 100;
        Ok(EffectiveSegmentation {
            enabled: self.enabled.unwrap_or(true),
            limits: SegmentLimits {
                max_segments: self
                    .max_segments
                    .unwrap_or(policy.defaults.max_segments)
                    .min(policy.max_segments),
                max_segment_bytes: policy.defaults.max_segment_bytes,
                max_pause_ms: scaled.min(policy.max_pause_ms),
            },
            pause_percent,
        })
    }
}

/// 宿主固定的默认预算与用户可选择的上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentPolicy {
    pub defaults: SegmentLimits,
    pub max_segments: usize,
    pub max_pause_ms: u64,
}
impl SegmentPolicy {
    /// 没有额外上限：用户只能在默认预算内收窄。
    pub fn fixed(defaults: SegmentLimits) -> Self {
        Self {
            defaults,
            max_segments: defaults.max_segments,
            max_pause_ms: defaults.max_pause_ms,
        }
    }
    pub fn validate(&self) -> SegmentResult<()> {
        self.defaults.validate()?;
        if (self.defaults.max_segments..=MAX_SEGMENTS).contains(&self.max_segments)
            && (self.defaults.max_pause_ms..=MAX_PAUSE_MS).contains(&self.max_pause_ms)
        {
            Ok(())
        } else {
            Err(SegmentError::InvalidLimits)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveSegmentation {
    pub enabled: bool,
    pub limits: SegmentLimits,
    pub pause_percent: u16,
}

/// 可信宿主持有的会话分段设置。不得发布到通用服务目录或交给模型、不受信插件。
/// 只保存用户明确命令；不从自然语言、记忆偏好或模型输出推断。
pub trait SegmentPreferences: Send + Sync {
    /// 未设置时返回 `SegmentPreference::default()`。
    fn get(&self, scope: &SegmentScope) -> SegmentPreferenceResult<SegmentPreference>;
    /// 原子读改写：先持久化再更新内存并返回新值；值未变化时不写入。
    /// 持久化报错时后端可能已提交，实现必须关闭自身，之后返回 Unavailable，直到重新打开。
    fn update(
        &self,
        scope: &SegmentScope,
        change: SegmentChange,
    ) -> SegmentPreferenceResult<SegmentPreference>;
}

/// 组合层入口：关闭时不调用规划器，整条发送；否则按有效段数规划，
/// 规划器的停顿只受宿主绝对上限约束，随后统一按百分比缩放一次并截到有效上限，
/// 最后重新校验计划。未设置偏好时与直接调用 `plan_or_single(defaults)` 等价。
pub fn plan_with_preference(
    planner: &dyn SegmentPlanner,
    text: &str,
    policy: &SegmentPolicy,
    preference: &SegmentPreference,
) -> SegmentResult<(SegmentPlan, Option<SegmentError>)> {
    let effective = preference.effective(policy)?;
    if !effective.enabled {
        let whole = SegmentPlan::single(FALLBACK_PLANNER, text)?;
        whole.validate(text, &effective.limits)?;
        return Ok((whole, None));
    }
    let planning = SegmentLimits {
        max_pause_ms: policy.max_pause_ms,
        ..effective.limits
    };
    let (mut plan, warning) = plan_or_single(planner, text, planning)?;
    for segment in plan.segments.iter_mut().skip(1) {
        segment.pause_before_ms = (segment.pause_before_ms * u64::from(effective.pause_percent)
            / 100)
            .min(effective.limits.max_pause_ms);
    }
    plan.validate(text, &effective.limits)?;
    Ok((plan, warning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Segment, SegmentRequest};
    use std::sync::Mutex;

    const DEFAULTS: SegmentLimits = SegmentLimits {
        max_segments: 3,
        max_segment_bytes: 32768,
        max_pause_ms: 2500,
    };
    const POLICY: SegmentPolicy = SegmentPolicy {
        defaults: DEFAULTS,
        max_segments: 5,
        max_pause_ms: 5000,
    };
    const TEXT: &str = "一。\n\n二。\n\n三。\n\n四。\n\n五。";

    /// 按收到的预算逐段切分，并给出固定停顿，记录实际收到的预算。
    #[derive(Default)]
    struct Recording(Mutex<Vec<SegmentLimits>>);
    impl SegmentPlanner for Recording {
        fn plan(&self, request: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
            self.0.lock().unwrap().push(request.limits);
            let parts: Vec<_> = request.text.split("\n\n").collect();
            let keep = request.limits.max_segments.min(parts.len());
            let mut offset = 0;
            let mut segments = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                if index < keep {
                    segments.push(Segment {
                        start: offset,
                        end: offset + part.len(),
                        pause_before_ms: if index == 0 { 0 } else { 2000 },
                    });
                } else {
                    segments.last_mut().unwrap().end = offset + part.len();
                }
                offset += part.len() + 2;
            }
            Ok(SegmentPlan {
                planner: "recording".into(),
                segments,
            })
        }
    }
    struct Panics;
    impl SegmentPlanner for Panics {
        fn plan(&self, _: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
            panic!("disabled preference must not plan")
        }
    }
    fn preference(
        enabled: Option<bool>,
        max_segments: Option<usize>,
        pause_percent: Option<u16>,
    ) -> SegmentPreference {
        SegmentPreference {
            enabled,
            max_segments,
            pause_percent,
        }
    }

    #[test]
    fn default_preference_keeps_host_defaults_exactly() {
        let planner = Recording::default();
        let (plan, warning) =
            plan_with_preference(&planner, TEXT, &POLICY, &SegmentPreference::default()).unwrap();
        assert!(warning.is_none());
        assert_eq!(
            planner.0.lock().unwrap()[0].max_segments,
            DEFAULTS.max_segments
        );
        assert_eq!(plan.segments.len(), 3);
        assert!(plan.segments[1..].iter().all(|s| s.pause_before_ms == 2000));
        // 没有额外上限的策略与直接按默认预算规划完全一致。
        let fixed = Recording::default();
        let direct = plan_or_single(&fixed, TEXT, DEFAULTS).unwrap();
        let via = plan_with_preference(
            &fixed,
            TEXT,
            &SegmentPolicy::fixed(DEFAULTS),
            &SegmentPreference::default(),
        )
        .unwrap();
        assert_eq!(direct, via);
        assert_eq!(fixed.0.lock().unwrap()[1], DEFAULTS);
    }

    #[test]
    fn parts_and_pace_stay_within_host_caps() {
        let planner = Recording::default();
        for (wanted, seen) in [(2, 2), (5, 5), (8, 5)] {
            let (plan, _) = plan_with_preference(
                &planner,
                TEXT,
                &POLICY,
                &preference(None, Some(wanted), None),
            )
            .unwrap();
            assert_eq!(planner.0.lock().unwrap().last().unwrap().max_segments, seen);
            assert_eq!(plan.segments.len(), seen);
        }
        // 停顿只缩放一次：50% 时 2000 变为 1000（上限 1250），200% 时为 4000（上限 5000）。
        for (percent, pause) in [(0, 0), (50, 1000), (100, 2000), (200, 4000)] {
            let (plan, _) = plan_with_preference(
                &planner,
                TEXT,
                &POLICY,
                &preference(None, None, Some(percent)),
            )
            .unwrap();
            assert_eq!(plan.segments[0].pause_before_ms, 0);
            assert!(
                plan.segments[1..]
                    .iter()
                    .all(|s| s.pause_before_ms == pause)
            );
            assert_eq!(planner.0.lock().unwrap().last().unwrap().max_pause_ms, 5000);
        }
        let effective = preference(None, None, Some(50)).effective(&POLICY).unwrap();
        assert_eq!(effective.limits.max_pause_ms, 1250);
        let fixed = SegmentPolicy::fixed(DEFAULTS);
        for max_segments in MIN_PREFERRED_SEGMENTS..=MAX_SEGMENTS {
            for percent in [0, 1, 99, 100, 101, 200] {
                let effective = preference(Some(true), Some(max_segments), Some(percent))
                    .effective(&fixed)
                    .unwrap();
                assert!(effective.limits.max_segments <= DEFAULTS.max_segments);
                assert!(effective.limits.max_pause_ms <= DEFAULTS.max_pause_ms);
                assert_eq!(
                    effective.limits.max_segment_bytes,
                    DEFAULTS.max_segment_bytes
                );
            }
        }
    }

    #[test]
    fn disabled_preference_never_calls_the_planner() {
        let (plan, warning) = plan_with_preference(
            &Panics,
            TEXT,
            &POLICY,
            &preference(Some(false), Some(5), None),
        )
        .unwrap();
        assert_eq!(
            (plan.planner.as_str(), plan.segments.len()),
            (FALLBACK_PLANNER, 1)
        );
        assert!(warning.is_none());
    }

    #[test]
    fn invalid_values_scopes_and_policies_are_rejected() {
        for bad in [
            preference(None, Some(0), None),
            preference(None, Some(1), None),
            preference(None, Some(MAX_SEGMENTS + 1), None),
            preference(None, None, Some(MAX_PAUSE_PERCENT + 1)),
        ] {
            assert_eq!(bad.validate(), Err(SegmentPreferenceError::InvalidInput));
            assert_eq!(bad.effective(&POLICY), Err(SegmentError::InvalidLimits));
        }
        let base = SegmentPreference::default();
        assert_eq!(
            base.apply(SegmentChange::MaxSegments(1)),
            Err(SegmentPreferenceError::InvalidInput)
        );
        let set = base.apply(SegmentChange::Enabled(true)).unwrap();
        assert!(!set.is_default());
        assert!(set.apply(SegmentChange::Reset).unwrap().is_default());
        for policy in [
            SegmentPolicy {
                max_segments: 2,
                ..POLICY
            },
            SegmentPolicy {
                max_segments: MAX_SEGMENTS + 1,
                ..POLICY
            },
            SegmentPolicy {
                max_pause_ms: 1000,
                ..POLICY
            },
            SegmentPolicy {
                max_pause_ms: MAX_PAUSE_MS + 1,
                ..POLICY
            },
        ] {
            assert_eq!(policy.validate(), Err(SegmentError::InvalidLimits));
        }
        let scope = |c: &str, s: &str, u: &str| SegmentScope {
            channel: c.into(),
            session_id: s.into(),
            user_id: u.into(),
        };
        scope("qq", "s", &"u".repeat(256)).validate().unwrap();
        for bad in [
            scope("", "s", "u"),
            scope("qq", " s", "u"),
            scope("qq", "s", "u "),
            scope("qq", "s\n", "u"),
            scope("qq", "s", &"u".repeat(257)),
        ] {
            assert_eq!(bad.validate(), Err(SegmentPreferenceError::InvalidInput));
        }
        assert_eq!(
            format!("{:?}", scope("qq", "secret", "u")),
            "SegmentScope(<redacted>)"
        );
    }
}
