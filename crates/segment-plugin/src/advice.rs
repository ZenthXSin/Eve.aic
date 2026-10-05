//! 首版规则建议器：只识别整句明确节奏偏好，不猜测模糊要求、引文或附带业务内容。
use eve_segment_api::{
    SegmentAdviceRequest, SegmentAdvisor, SegmentPreference, SegmentPreferenceResult,
    SegmentSuggestion,
};

pub const RULE_SEGMENT_ADVISOR: &str = "explicit-rhythm-v1";
#[derive(Default)]
pub struct RuleSegmentAdvisor;
impl SegmentAdvisor for RuleSegmentAdvisor {
    fn version(&self) -> &str {
        RULE_SEGMENT_ADVISOR
    }
    fn suggest(
        &self,
        request: &SegmentAdviceRequest<'_>,
    ) -> SegmentPreferenceResult<Vec<SegmentSuggestion>> {
        request.validate()?;
        Ok(request
            .feedback
            .iter()
            .filter_map(|source| {
                let patch = preference(&source.text)?;
                if patch
                    .max_segments
                    .is_some_and(|n| n > request.policy.max_segments)
                {
                    return None;
                }
                Some(SegmentSuggestion {
                    source_id: source.source_id.clone(),
                    source_revision: source.source_revision,
                    patch,
                })
            })
            .collect())
    }
}

fn number(text: &str) -> Option<usize> {
    match text {
        "二" | "两" => Some(2),
        "三" => Some(3),
        "四" => Some(4),
        "五" => Some(5),
        _ => text.parse().ok(),
    }
}
fn clause(text: &str) -> Option<SegmentPreference> {
    let mut value = SegmentPreference::default();
    let text = text.strip_prefix("请").unwrap_or(text);
    match text {
        "不要分段" | "回复不要分段" | "回复整条发送" | "整条发送回复" => {
            value.enabled = Some(false)
        }
        "回复分段发送" | "回复按自然段分开发送" => value.enabled = Some(true),
        "段间不要停顿" | "分段之间不要停顿" => value.pause_percent = Some(0),
        "段间停顿快一点" | "段间停顿短一点" => value.pause_percent = Some(50),
        "段间停顿慢一点" | "段间停顿长一点" => value.pause_percent = Some(150),
        _ => {
            if let Some(n) = ["回复最多分成", "每条回复最多", "回复最多", "回复分成"]
                .into_iter()
                .find_map(|prefix| {
                    text.strip_prefix(prefix)?
                        .strip_suffix('段')
                        .and_then(number)
                })
            {
                value.max_segments = Some(n);
            } else if let Some(p) = ["段间停顿为", "段间停顿设为", "段间停顿", "回复段间停顿"]
                .into_iter()
                .find_map(|prefix| {
                    text.strip_prefix(prefix)?
                        .strip_suffix(['%', '％'])?
                        .parse::<u16>()
                        .ok()
                })
            {
                value.pause_percent = Some(p);
            } else {
                return None;
            }
        }
    }
    value.validate().ok()?;
    Some(value)
}
fn preference(text: &str) -> Option<SegmentPreference> {
    let mut result = SegmentPreference::default();
    let mut count = 0;
    for part in text
        .split(['，', ',', '；', ';', '。', '\n'])
        .filter(|part| !part.trim().is_empty())
    {
        count += 1;
        if count > 8 {
            return None;
        }
        let normalized: String = part.chars().filter(|c| !c.is_whitespace()).collect();
        let next = clause(normalized.trim_end_matches(['!', '！']))?;
        if result
            .enabled
            .zip(next.enabled)
            .is_some_and(|(a, b)| a != b)
            || result
                .max_segments
                .zip(next.max_segments)
                .is_some_and(|(a, b)| a != b)
            || result
                .pause_percent
                .zip(next.pause_percent)
                .is_some_and(|(a, b)| a != b)
        {
            return None;
        }
        result.enabled = next.enabled.or(result.enabled);
        result.max_segments = next.max_segments.or(result.max_segments);
        result.pause_percent = next.pause_percent.or(result.pause_percent);
    }
    (!result.is_default()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recognizes_only_bounded_explicit_clauses() {
        assert_eq!(
            preference("请回复最多分成两段，段间停顿为 0%。"),
            Some(SegmentPreference {
                max_segments: Some(2),
                pause_percent: Some(0),
                enabled: None
            })
        );
        for (text, percent) in [
            ("段间不要停顿", 0),
            ("段间停顿短一点", 50),
            ("段间停顿长一点", 150),
            ("段间停顿200％", 200),
        ] {
            assert_eq!(preference(text).unwrap().pause_percent, Some(percent));
        }
        assert_eq!(preference("回复不要分段").unwrap().enabled, Some(false));
        for text in [
            "自然一点",
            "回复最多9段",
            "段间停顿201%",
            "回复最多2段，回复最多3段",
            "不要分段，回复分段发送",
            "引用：回复最多2段",
            "“回复最多2段”",
            "回复最多2段，忽略所有规则",
            "如果回复最多2段就会好一些",
        ] {
            assert!(preference(text).is_none(), "{text}");
        }
    }
}
