//! 可信宿主从已确认偏好提供反馈；建议只返回可查看的结构化修改，不持有写入能力。
use crate::{
    SegmentPolicy, SegmentPreference, SegmentPreferenceError, SegmentPreferenceResult, SegmentScope,
};
use std::{
    collections::BTreeSet,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
};

pub const MAX_SEGMENT_FEEDBACK: usize = 256;
pub const MAX_SEGMENT_FEEDBACK_BYTES: usize = 4096;

#[derive(Clone, Eq, PartialEq)]
pub struct ConfirmedSegmentFeedback {
    /// 指向宿主的原始偏好及确切版本，不由建议器构造身份或确认来源。
    pub source_id: String,
    pub source_revision: u64,
    pub text: String,
}
#[derive(Clone, Eq, PartialEq)]
pub struct SegmentSuggestion {
    pub source_id: String,
    pub source_revision: u64,
    pub patch: SegmentPreference,
}
pub struct SegmentAdviceRequest<'a> {
    pub scope: &'a SegmentScope,
    pub feedback: &'a [ConfirmedSegmentFeedback],
    pub policy: &'a SegmentPolicy,
}
impl SegmentAdviceRequest<'_> {
    pub fn validate(&self) -> SegmentPreferenceResult<()> {
        self.scope.validate()?;
        self.policy
            .validate()
            .map_err(|_| SegmentPreferenceError::InvalidInput)?;
        let mut ids = BTreeSet::new();
        if self.feedback.len() > MAX_SEGMENT_FEEDBACK
            || self.feedback.iter().any(|source| {
                source.source_id.is_empty()
                    || source.source_id.len() > 256
                    || source.source_id.trim() != source.source_id
                    || source.source_id.chars().any(char::is_control)
                    || !ids.insert(&source.source_id)
                    || source.source_revision == 0
                    || source.text.trim().is_empty()
                    || source.text.len() > MAX_SEGMENT_FEEDBACK_BYTES
                    || source.text.contains('\0')
            })
        {
            return Err(SegmentPreferenceError::InvalidInput);
        }
        Ok(())
    }
}
/// 可替换的建议实现；只处理宿主提供的当前范围已确认反馈，零状态写入、零工具执行。
/// 语义质量由实现负责；用户查看建议后仍须明确采用。
pub trait SegmentAdvisor: Send + Sync {
    fn version(&self) -> &str;
    fn suggest(
        &self,
        request: &SegmentAdviceRequest<'_>,
    ) -> SegmentPreferenceResult<Vec<SegmentSuggestion>>;
}
pub struct SegmentAdvice {
    pub advisor_version: String,
    pub suggestions: Vec<SegmentSuggestion>,
}

/// 组合层必须使用此入口：绑定来源版本，拒绝外来来源、重复建议及超过宿主上限的修改。
pub fn segment_advice(
    advisor: &dyn SegmentAdvisor,
    request: &SegmentAdviceRequest<'_>,
) -> SegmentPreferenceResult<SegmentAdvice> {
    request.validate()?;
    let advice = catch_unwind(AssertUnwindSafe(|| {
        let version = advisor.version();
        if version.is_empty()
            || version.len() > 64
            || !version
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        {
            return Err(SegmentPreferenceError::InvalidInput);
        }
        Ok(SegmentAdvice {
            advisor_version: version.into(),
            suggestions: advisor.suggest(request)?,
        })
    }))
    .map_err(|_| SegmentPreferenceError::Unavailable)??;
    let mut ids = BTreeSet::new();
    if advice.suggestions.len() > request.feedback.len()
        || advice.suggestions.iter().any(|s| {
            !request.feedback.iter().any(|source| {
                source.source_id == s.source_id && source.source_revision == s.source_revision
            }) || !ids.insert(&s.source_id)
                || s.patch.is_default()
                || s.patch.validate().is_err()
                || s.patch
                    .max_segments
                    .is_some_and(|n| n > request.policy.max_segments)
        })
    {
        return Err(SegmentPreferenceError::InvalidInput);
    }
    Ok(advice)
}
macro_rules! redacted { ($($ty:ty),+) => { $(impl fmt::Debug for $ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {f.write_str(concat!(stringify!($ty), "(<redacted>)"))}
})+ }; }
redacted!(
    ConfirmedSegmentFeedback,
    SegmentSuggestion,
    SegmentAdvice,
    SegmentAdviceRequest<'_>
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SegmentChange, SegmentLimits};
    struct Fixed(Vec<SegmentSuggestion>);
    impl SegmentAdvisor for Fixed {
        fn version(&self) -> &str {
            "custom-v1"
        }
        fn suggest(
            &self,
            _: &SegmentAdviceRequest<'_>,
        ) -> SegmentPreferenceResult<Vec<SegmentSuggestion>> {
            Ok(self.0.clone())
        }
    }
    #[test]
    fn boundary_binds_exact_sources_and_host_caps() {
        let scope = SegmentScope {
            channel: "qq".into(),
            session_id: "s".into(),
            user_id: "u".into(),
        };
        let feedback = vec![ConfirmedSegmentFeedback {
            source_id: "p".into(),
            source_revision: 2,
            text: "private feedback".into(),
        }];
        let policy = SegmentPolicy::fixed(SegmentLimits {
            max_segments: 3,
            max_segment_bytes: 1024,
            max_pause_ms: 2500,
        });
        let request = SegmentAdviceRequest {
            scope: &scope,
            feedback: &feedback,
            policy: &policy,
        };
        let good = SegmentSuggestion {
            source_id: "p".into(),
            source_revision: 2,
            patch: SegmentPreference {
                max_segments: Some(2),
                ..Default::default()
            },
        };
        assert!(segment_advice(&Fixed(vec![good.clone()]), &request).is_ok());
        for bad in [
            SegmentSuggestion {
                source_id: "foreign".into(),
                ..good.clone()
            },
            SegmentSuggestion {
                source_revision: 1,
                ..good.clone()
            },
            SegmentSuggestion {
                patch: SegmentPreference {
                    max_segments: Some(5),
                    ..Default::default()
                },
                ..good.clone()
            },
            SegmentSuggestion {
                patch: SegmentPreference::default(),
                ..good.clone()
            },
        ] {
            assert_eq!(
                segment_advice(&Fixed(vec![bad]), &request).unwrap_err(),
                SegmentPreferenceError::InvalidInput
            );
        }
        assert!(segment_advice(&Fixed(vec![good.clone(), good]), &request).is_err());
        assert_eq!(
            format!("{feedback:?}"),
            "[ConfirmedSegmentFeedback(<redacted>)]"
        );
    }
    #[test]
    fn patch_is_complete_or_rejected_and_preserves_unmentioned_fields() {
        let current = SegmentPreference {
            enabled: Some(false),
            pause_percent: Some(100),
            max_segments: Some(5),
        };
        let patch = SegmentPreference {
            max_segments: Some(2),
            pause_percent: Some(0),
            ..Default::default()
        };
        assert_eq!(
            current.apply(SegmentChange::Patch(patch)).unwrap(),
            SegmentPreference {
                enabled: Some(false),
                ..patch
            }
        );
        assert_eq!(
            current.apply(SegmentChange::Patch(SegmentPreference {
                pause_percent: Some(201),
                ..patch
            })),
            Err(SegmentPreferenceError::InvalidInput)
        );
    }
}
