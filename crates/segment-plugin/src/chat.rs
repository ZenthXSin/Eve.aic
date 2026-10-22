//! 聊天分条规划：短的闲聊回复按句子拆成几条短消息，段间停顿按下一条的字数模拟打字；
//! 含代码、列表、标题、表格或较长的回复交给自然段规划。只返回原文断点，不改写回复。
use crate::{Kind, ParagraphPlanner, classify, has_content, visible};
use eve_segment_api::{
    Segment, SegmentError, SegmentPlan, SegmentPlanner, SegmentRequest, SegmentResult,
    fenced_code_spans,
};
use std::{ops::Range, sync::Arc};

pub const CHAT_PLANNER: &str = "chat-v1";

/// 按句拆分的闲聊回复上限（非空白字符数）；更长的回复按自然段处理。
pub const MAX_CHAT_CHARS: usize = 240;
/// 整条少于该字数时不拆。
pub const MIN_SPLIT_CHARS: usize = 12;
/// 少于该字数的句子并入相邻句，避免“嗯。”单独成条。
const MIN_PIECE_CHARS: usize = 4;

const TERMINATORS: &[char] = &['。', '！', '？', '!', '?', '…', '～', '~'];
const CLOSERS: &[char] = &['”', '’', '」', '』', '）', ')', '】', '》', '"', '\''];
const OPENERS: &[(char, char)] = &[
    ('“', '”'),
    ('「', '」'),
    ('『', '』'),
    ('（', '）'),
    ('(', ')'),
    ('【', '】'),
    ('《', '》'),
];

/// 是否启用按句拆分；每条回复规划时读取，可热修改。关闭时等同自然段规划。
pub trait ChatSplitting: Send + Sync {
    fn sentences(&self) -> bool;
}

pub struct ChatPlanner {
    pub paragraph: ParagraphPlanner,
    pub splitting: Arc<dyn ChatSplitting>,
    /// 段前停顿 = 基础 + 每字 × 下一条字数，受本轮上限约束。
    pub base_pause_ms: u64,
    pub pause_per_char_ms: u64,
}

impl ChatPlanner {
    pub fn new(splitting: Arc<dyn ChatSplitting>) -> Self {
        Self {
            paragraph: ParagraphPlanner::default(),
            splitting,
            base_pause_ms: 300,
            pause_per_char_ms: 90,
        }
    }
}

/// 只有普通文字行的短回复才算闲聊；结构化内容保持段落完整。
fn casual(text: &str) -> bool {
    fenced_code_spans(text).is_empty()
        && visible(text) <= MAX_CHAT_CHARS
        && !text.contains("://")
        && text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .all(|line| classify(line) == Kind::Prose)
}

/// 在句末标点（连同紧随的引号、括号）及换行处断开；引号和括号内部不断开。
fn sentences(text: &str) -> Vec<Range<usize>> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut open: Vec<char> = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if let Some(&(_, close)) = OPENERS.iter().find(|(o, _)| *o == c) {
            open.push(close);
            continue;
        }
        if open.last() == Some(&c) {
            open.pop();
            // 引号或括号以句末标点收尾时，在闭合符之后断开。
            let closes_sentence =
                open.is_empty() && text[..index].ends_with(|p: char| TERMINATORS.contains(&p));
            if !closes_sentence {
                continue;
            }
            let end = index + c.len_utf8();
            pieces.push(start..end);
            start = end;
            continue;
        }
        let ascii_period = c == '.'
            && chars.peek().is_none_or(|(_, next)| next.is_whitespace())
            && !text[..index].ends_with(|p: char| p.is_ascii_digit());
        let boundary = c == '\n' || (open.is_empty() && (TERMINATORS.contains(&c) || ascii_period));
        if !boundary {
            continue;
        }
        let mut end = index + c.len_utf8();
        while let Some(&(next_index, next)) = chars.peek() {
            if TERMINATORS.contains(&next) || (c != '\n' && CLOSERS.contains(&next)) {
                end = next_index + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        if c == '\n' {
            open.clear();
        }
        pieces.push(start..end);
        start = end;
    }
    pieces.push(start..text.len());
    pieces
        .into_iter()
        .map(|range| {
            let slice = &text[range.clone()];
            let lead = slice.len() - slice.trim_start().len();
            range.start + lead..range.start + slice.trim_end().len()
        })
        .filter(|range| !range.is_empty())
        .collect()
}

impl SegmentPlanner for ChatPlanner {
    fn plan(&self, request: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
        let text = request.text;
        let limits = request.limits;
        if !self.splitting.sentences() || !casual(text) {
            return self.paragraph.plan(request);
        }
        if visible(text) < MIN_SPLIT_CHARS {
            return SegmentPlan::single(CHAT_PLANNER, text);
        }
        let mut units = sentences(text);
        let mut counts: Vec<usize> = units.iter().map(|u| visible(&text[u.clone()])).collect();
        // 过短或只有标点、表情的句子并入前一句（首句并入后一句）。
        let mut index = 0;
        while index < units.len() && units.len() > 1 {
            let short =
                counts[index] < MIN_PIECE_CHARS || !has_content(&text[units[index].clone()]);
            if !short {
                index += 1;
                continue;
            }
            let target = if index == 0 { 1 } else { index - 1 };
            let (left, right) = (target.min(index), target.max(index));
            units[left].end = units[right].end;
            counts[left] += counts[right];
            units.remove(right);
            counts.remove(right);
            index = left;
        }
        while units.len() > limits.max_segments {
            let best = (0..units.len() - 1)
                .filter(|i| units[*i + 1].end - units[*i].start <= limits.max_segment_bytes)
                .min_by_key(|i| (counts[*i] + counts[*i + 1], usize::MAX - i))
                .ok_or_else(|| SegmentError::Planner("无法在单段预算内合并".into()))?;
            units[best].end = units[best + 1].end;
            counts[best] += counts.remove(best + 1);
            units.remove(best + 1);
        }
        if units.len() <= 1 {
            return SegmentPlan::single(CHAT_PLANNER, text);
        }
        let segments = units
            .iter()
            .zip(&counts)
            .enumerate()
            .map(|(index, (unit, count))| Segment {
                start: unit.start,
                end: unit.end,
                pause_before_ms: if index == 0 {
                    0
                } else {
                    self.base_pause_ms
                        .saturating_add(
                            self.pause_per_char_ms
                                .saturating_mul(u64::try_from(*count).unwrap_or(u64::MAX)),
                        )
                        .min(limits.max_pause_ms)
                },
            })
            .collect();
        Ok(SegmentPlan {
            planner: CHAT_PLANNER.into(),
            segments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_segment_api::{SegmentLimits, plan_or_single};

    struct On(bool);
    impl ChatSplitting for On {
        fn sentences(&self) -> bool {
            self.0
        }
    }
    const LIMITS: SegmentLimits = SegmentLimits {
        max_segments: 3,
        max_segment_bytes: 32768,
        max_pause_ms: 2500,
    };
    fn texts(planner: &ChatPlanner, text: &str) -> Vec<String> {
        let (plan, error) = plan_or_single(planner, text, LIMITS).unwrap();
        assert_eq!(error, None);
        plan.texts(text).map(str::to_owned).collect()
    }

    #[test]
    fn casual_reply_splits_into_short_messages_and_keeps_quotes_whole() {
        let planner = ChatPlanner::new(Arc::new(On(true)));
        assert_eq!(
            texts(
                &planner,
                "哈哈，我也觉得。他说“今天不去了！明天再说。”你怎么看？"
            ),
            [
                "哈哈，我也觉得。",
                "他说“今天不去了！明天再说。”",
                "你怎么看？"
            ]
        );
        // 过短的句子并入相邻句。
        assert_eq!(
            texts(&planner, "嗯。这个我昨天刚看过，挺有意思的。"),
            ["嗯。这个我昨天刚看过，挺有意思的。"]
        );
        // 超过段数时合并最短的相邻两条。
        let parts = texts(
            &planner,
            "第一句比较长一些。第二句。第三句也稍微长一点点。第四句收个尾吧。",
        );
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts.concat(),
            "第一句比较长一些。第二句。第三句也稍微长一点点。第四句收个尾吧。"
        );
        // 小数与网址里的点不断开。
        assert_eq!(texts(&planner, "版本是 3.14 吧"), ["版本是 3.14 吧"]);
    }

    #[test]
    fn pauses_follow_the_next_message_length_and_the_cap() {
        let planner = ChatPlanner::new(Arc::new(On(true)));
        let text = "先说结论，可以用。不过要注意内存占用会比较高，建议先在小数据上试一下。";
        let (plan, _) = plan_or_single(&planner, text, LIMITS).unwrap();
        assert_eq!(plan.planner, CHAT_PLANNER);
        assert_eq!(plan.segments[0].pause_before_ms, 0);
        assert_eq!(plan.segments[1].pause_before_ms, 2500);
        let short = "好的，那就这样定了。明天见。";
        let (plan, _) = plan_or_single(&planner, short, LIMITS).unwrap();
        assert_eq!(plan.segments[1].pause_before_ms, 300 + 90 * 4);
    }

    #[test]
    fn structured_long_or_disabled_replies_use_paragraph_planning() {
        let on = ChatPlanner::new(Arc::new(On(true)));
        let list = "步骤如下：\n\n1. 安装。\n2. 运行。";
        let (plan, _) = plan_or_single(&on, list, LIMITS).unwrap();
        assert_eq!(plan.planner, crate::PARAGRAPH_PLANNER);
        let code = "看这里。\n\n```\nlet x = 1;\n```";
        assert_eq!(
            plan_or_single(&on, code, LIMITS).unwrap().0.planner,
            crate::PARAGRAPH_PLANNER
        );
        let long = "很长的一句话。".repeat(40);
        assert_eq!(
            plan_or_single(&on, &long, LIMITS).unwrap().0.planner,
            crate::PARAGRAPH_PLANNER
        );
        let off = ChatPlanner::new(Arc::new(On(false)));
        let casual = "哈哈，我也觉得。你怎么看这个问题？";
        assert_eq!(
            plan_or_single(&off, casual, LIMITS).unwrap().0.planner,
            crate::PARAGRAPH_PLANNER
        );
        assert_eq!(texts(&on, "好的"), ["好的"]);
    }
}
