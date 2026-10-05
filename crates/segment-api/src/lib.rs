//! 同一轮回复的分段计划契约：只在原文中选择断点与段前停顿，不改写、补写或删除回复。
//! 分段不是新输入，也不是新的执行事实；Session 仍保存完整回复，投递回执由通道记录。
use std::{
    fmt,
    ops::Range,
    panic::{AssertUnwindSafe, catch_unwind},
};

/// 契约硬上限；通道可以更小，例如受平台被动回复次数约束。
pub const MAX_SEGMENTS: usize = 8;
pub const MAX_TEXT_BYTES: usize = 32_768;
pub const MAX_PAUSE_MS: u64 = 10_000;
const MAX_PLANNER_BYTES: usize = 64;

pub type SegmentResult<T> = Result<T, SegmentError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SegmentError {
    InvalidLimits,
    InvalidText,
    /// 计划违反契约；只给出固定类别，不回显回复正文。
    InvalidPlan(&'static str),
    /// 规划器自身失败或 panic；组合层降级为整条回复。
    Planner(String),
}
impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("分段预算无效"),
            Self::InvalidText => f.write_str("待分段回复为空或超过上限"),
            Self::InvalidPlan(reason) => write!(f, "分段计划无效：{reason}"),
            Self::Planner(reason) => write!(f, "分段规划失败：{reason}"),
        }
    }
}
impl std::error::Error for SegmentError {}

/// 宿主为本轮固定的预算；规划器不能放宽。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SegmentLimits {
    pub max_segments: usize,
    pub max_segment_bytes: usize,
    pub max_pause_ms: u64,
}
impl SegmentLimits {
    pub fn validate(&self) -> SegmentResult<()> {
        if (1..=MAX_SEGMENTS).contains(&self.max_segments)
            && (1..=MAX_TEXT_BYTES).contains(&self.max_segment_bytes)
            && self.max_pause_ms <= MAX_PAUSE_MS
        {
            Ok(())
        } else {
            Err(SegmentError::InvalidLimits)
        }
    }
}

/// 原回复中的 UTF-8 字节范围；首段停顿必须为 0。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    pub start: usize,
    pub end: usize,
    pub pause_before_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SegmentPlan {
    /// 规划器名称与版本，供回执审计；不含正文。
    pub planner: String,
    pub segments: Vec<Segment>,
}
impl SegmentPlan {
    /// 整条发送：去掉首尾空白后的完整回复。
    pub fn single(planner: impl Into<String>, text: &str) -> SegmentResult<Self> {
        let start = text.len() - text.trim_start().len();
        let end = text.trim_end().len();
        let plan = Self {
            planner: planner.into(),
            segments: vec![Segment {
                start,
                end: end.max(start),
                pause_before_ms: 0,
            }],
        };
        validate_text(text)?;
        validate_planner(&plan.planner)?;
        Ok(plan)
    }
    pub fn texts<'a>(&'a self, text: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.segments.iter().map(move |s| &text[s.start..s.end])
    }
    /// 片段按序覆盖全部非空白内容：片段之间只能是空白，每段首尾不含空白，
    /// 不切开围栏代码块，并满足本轮预算。
    pub fn validate(&self, text: &str, limits: &SegmentLimits) -> SegmentResult<()> {
        limits.validate()?;
        validate_text(text)?;
        validate_planner(&self.planner)?;
        if self.segments.is_empty() || self.segments.len() > limits.max_segments {
            return Err(SegmentError::InvalidPlan("片段数量超出预算"));
        }
        validate_ranges(
            text,
            self.segments.iter().map(|s| s.start..s.end),
            limits.max_segment_bytes,
        )?;
        for (index, segment) in self.segments.iter().enumerate() {
            let allowed = if index == 0 { 0 } else { limits.max_pause_ms };
            if segment.pause_before_ms > allowed {
                return Err(SegmentError::InvalidPlan("停顿超出预算"));
            }
        }
        Ok(())
    }
}

/// 同一检查也供通道恢复持久回执时使用：范围必须按序、位于字符边界、
/// 只隔着空白、自身不以空白开头或结尾，且不切开围栏代码块。
pub fn validate_ranges(
    text: &str,
    ranges: impl IntoIterator<Item = Range<usize>>,
    max_segment_bytes: usize,
) -> SegmentResult<()> {
    let mut cursor = 0;
    let mut covered = Vec::new();
    for range in ranges {
        let Range { start, end } = range;
        if start < cursor
            || start >= end
            || end > text.len()
            || !text.is_char_boundary(start)
            || !text.is_char_boundary(end)
        {
            return Err(SegmentError::InvalidPlan("片段范围越界或乱序"));
        }
        if !text[cursor..start].trim().is_empty() {
            return Err(SegmentError::InvalidPlan("片段遗漏了原文内容"));
        }
        let slice = &text[start..end];
        if slice.trim() != slice {
            return Err(SegmentError::InvalidPlan("片段首尾含空白"));
        }
        // 通道桥接（JavaScript trim）还把 U+FEFF 视为空白；只含它的片段无法投递。
        if slice.chars().all(|c| c.is_whitespace() || c == '\u{feff}') {
            return Err(SegmentError::InvalidPlan("片段没有可见内容"));
        }
        if slice.len() > max_segment_bytes {
            return Err(SegmentError::InvalidPlan("单段超过字节预算"));
        }
        covered.push(start..end);
        cursor = end;
    }
    if covered.is_empty() || !text[cursor..].trim().is_empty() {
        return Err(SegmentError::InvalidPlan("片段遗漏了原文内容"));
    }
    for span in fenced_code_spans(text) {
        if !covered
            .iter()
            .any(|c| c.start <= span.start && span.end <= c.end)
        {
            return Err(SegmentError::InvalidPlan("片段切开了代码块"));
        }
    }
    Ok(())
}

/// 围栏代码块的字节范围：从开围栏首个标记到闭围栏最后一个标记；
/// 未闭合时延伸到最后一个非空白字符，整体不可拆分。
/// 有意从宽识别：任意缩进（含列表内嵌套）的围栏都算，多识别只会减少断点。
/// 反引号围栏的信息串不能含反引号，因此行首的 ```x``` 是行内代码而非围栏。
pub fn fenced_code_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut open: Option<(usize, char, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let indent = content.len() - content.trim_start_matches([' ', '\t']).len();
        let body = &content[indent..];
        let marker = body.chars().next().filter(|c| *c == '`' || *c == '~');
        let run = marker.map_or(0, |c| body.len() - body.trim_start_matches(c).len());
        if run >= 3 {
            let c = marker.expect("run implies marker");
            let info = &body[run..];
            match open {
                None if c == '~' || !info.contains('`') => open = Some((offset + indent, c, run)),
                Some((start, open_char, open_run))
                    if c == open_char && run >= open_run && info.trim().is_empty() =>
                {
                    spans.push(start..offset + indent + run);
                    open = None;
                }
                _ => {}
            }
        }
        offset += line.len();
    }
    if let Some((start, _, _)) = open {
        spans.push(start..text.trim_end().len().max(start));
    }
    spans
}

fn validate_text(text: &str) -> SegmentResult<()> {
    if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
        Err(SegmentError::InvalidText)
    } else {
        Ok(())
    }
}
fn validate_planner(planner: &str) -> SegmentResult<()> {
    if !planner.is_empty()
        && planner.len() <= MAX_PLANNER_BYTES
        && planner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(SegmentError::InvalidPlan("规划器标识无效"))
    }
}

pub struct SegmentRequest<'a> {
    pub text: &'a str,
    pub limits: SegmentLimits,
}

/// 可替换的分段策略。实现只返回断点和停顿，不发送消息、不访问 Session 或模型工具。
pub trait SegmentPlanner: Send + Sync {
    fn plan(&self, request: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan>;
}

pub const FALLBACK_PLANNER: &str = "single";

/// 组合层入口：规划失败、panic 或计划无效时降级为整条回复，并返回原因供告警。
/// 回复或预算无效、或者规划失败且整条回复也无法满足单段预算时返回错误。
pub fn plan_or_single(
    planner: &dyn SegmentPlanner,
    text: &str,
    limits: SegmentLimits,
) -> SegmentResult<(SegmentPlan, Option<SegmentError>)> {
    limits.validate()?;
    validate_text(text)?;
    let request = SegmentRequest { text, limits };
    let planned = catch_unwind(AssertUnwindSafe(|| planner.plan(&request)))
        .unwrap_or_else(|_| Err(SegmentError::Planner("规划器 panic".into())))
        .and_then(|plan| plan.validate(text, &limits).map(|()| plan));
    Ok(match planned {
        Ok(plan) => (plan, None),
        Err(error) => {
            let fallback = SegmentPlan::single(FALLBACK_PLANNER, text)?;
            fallback.validate(text, &limits)?;
            (fallback, Some(error))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: SegmentLimits = SegmentLimits {
        max_segments: 3,
        max_segment_bytes: 1024,
        max_pause_ms: 2000,
    };
    fn plan(ranges: &[(usize, usize, u64)]) -> SegmentPlan {
        SegmentPlan {
            planner: "test-v1".into(),
            segments: ranges
                .iter()
                .map(|&(start, end, pause_before_ms)| Segment {
                    start,
                    end,
                    pause_before_ms,
                })
                .collect(),
        }
    }

    #[test]
    fn accepts_whitespace_separated_partition_and_rejects_lost_or_altered_content() {
        let text = "\n结论。\n\n解释一下。\n";
        let first = text.find("结论").unwrap();
        let second = text.find("解释").unwrap();
        let end = text.trim_end().len();
        plan(&[(first, first + "结论。".len(), 0), (second, end, 500)])
            .validate(text, &LIMITS)
            .unwrap();
        assert_eq!(
            plan(&[(first, first + "结论".len(), 0), (second, end, 500)]).validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段遗漏了原文内容"))
        );
        assert_eq!(
            plan(&[(first, first + "结论。".len(), 0)]).validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段遗漏了原文内容"))
        );
        assert_eq!(
            plan(&[(0, end, 0)]).validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段首尾含空白"))
        );
        assert_eq!(
            plan(&[(first + 1, end, 0)]).validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段范围越界或乱序"))
        );
        let head = first + "结论。".len();
        assert_eq!(
            plan(&[(first, head, 0), (first, head, 0), (second, end, 0)]).validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段范围越界或乱序"))
        );
    }

    #[test]
    fn enforces_budget_first_pause_and_planner_identity() {
        let text = "一。\n\n二。\n\n三。\n\n四。";
        let ranges: Vec<_> = text
            .split("\n\n")
            .scan(0, |offset, part| {
                let start = *offset;
                *offset += part.len() + 2;
                Some((start, start + part.len(), 100))
            })
            .collect();
        let mut over = plan(&ranges);
        over.segments[0].pause_before_ms = 0;
        assert_eq!(
            over.validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段数量超出预算"))
        );
        let mut first_pause = plan(&ranges[..1]);
        first_pause.segments[0].end = text.len();
        assert_eq!(
            first_pause.validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("停顿超出预算"))
        );
        let mut long_pause = plan(&[(0, 6, 0), (8, text.len(), 2001)]);
        assert_eq!(
            long_pause.validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("停顿超出预算"))
        );
        long_pause.segments[1].pause_before_ms = 2000;
        long_pause.planner = "带空格 v1".into();
        assert_eq!(
            long_pause.validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("规划器标识无效"))
        );
        let tight = SegmentLimits {
            max_segment_bytes: 5,
            ..LIMITS
        };
        assert_eq!(
            SegmentPlan::single("single", text)
                .unwrap()
                .validate(text, &tight),
            Err(SegmentError::InvalidPlan("单段超过字节预算"))
        );
        assert_eq!(
            SegmentLimits {
                max_segments: MAX_SEGMENTS + 1,
                ..LIMITS
            }
            .validate(),
            Err(SegmentError::InvalidLimits)
        );
    }

    #[test]
    fn code_fences_cannot_be_split_even_with_blank_lines_inside() {
        let text = "看这里：\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\n完。";
        let spans = fenced_code_spans(text);
        assert_eq!(spans.len(), 1);
        assert!(text[spans[0].clone()].starts_with("```rust"));
        assert!(text[spans[0].clone()].ends_with("```"));
        let inner = text.find("fn b").unwrap();
        let split = plan(&[(0, inner - 2, 0), (inner, text.len(), 100)]);
        assert_eq!(
            split.validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段切开了代码块"))
        );
        let fence_end = spans[0].end;
        let tail = text.find("完").unwrap();
        plan(&[(0, fence_end, 0), (tail, text.len(), 100)])
            .validate(text, &LIMITS)
            .unwrap();
    }

    #[test]
    fn fence_scanner_handles_tildes_longer_closers_and_unclosed_blocks() {
        let text = "~~~~\n```\nstill code\n~~~~~\nafter\n```py\nopen";
        let spans = fenced_code_spans(text);
        assert_eq!(spans.len(), 2);
        assert!(text[spans[0].clone()].contains("still code"));
        assert!(text[spans[0].clone()].ends_with("~~~~~"));
        assert_eq!(&text[spans[1].clone()], "```py\nopen");
    }

    #[test]
    fn nested_or_indented_fences_are_protected_and_inline_backticks_are_not_fences() {
        let nested = "步骤：\n\n1. 安装：\n    ```bash\n    npm install\n\n    npm run build\n    ```\n2. 启动。";
        let spans = fenced_code_spans(nested);
        assert_eq!(spans.len(), 1);
        assert!(nested[spans[0].clone()].contains("npm install\n\n    npm run build"));
        let inline = "```ls``` 列出文件：\n\n```\necho a\n\necho b\n```\n\n完。";
        let spans = fenced_code_spans(inline);
        assert_eq!(spans.len(), 1);
        assert_eq!(&inline[spans[0].clone()], "```\necho a\n\necho b\n```");
        let tilde = "~~~ `info` 可以含反引号\ncode\n~~~";
        assert_eq!(fenced_code_spans(tilde), vec![0..tilde.len()]);
    }

    #[test]
    fn segments_need_content_visible_to_the_channel_bridge() {
        let text = "第一段内容。\n\n\u{feff}\u{feff}\n\n第二段内容。";
        let second = text.find('\u{feff}').unwrap();
        let third = text.find("第二").unwrap();
        let first_end = text.find("\n\n").unwrap();
        assert_eq!(
            plan(&[
                (0, first_end, 0),
                (second, second + 6, 100),
                (third, text.len(), 100)
            ])
            .validate(text, &LIMITS),
            Err(SegmentError::InvalidPlan("片段没有可见内容"))
        );
        plan(&[(0, first_end, 0), (second, text.len(), 100)])
            .validate(text, &LIMITS)
            .unwrap();
    }

    struct Fixed(SegmentResult<SegmentPlan>);
    impl SegmentPlanner for Fixed {
        fn plan(&self, _: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
            self.0.clone()
        }
    }
    struct Panics;
    impl SegmentPlanner for Panics {
        fn plan(&self, _: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
            panic!("planner bug")
        }
    }

    #[test]
    fn composition_accepts_valid_parts_when_the_whole_reply_exceeds_one_part_budget() {
        let text = "第一段。\n\n第二段。";
        let second = text.find("第二段").unwrap();
        let limits = SegmentLimits {
            max_segments: 2,
            max_segment_bytes: "第一段。".len(),
            max_pause_ms: 1000,
        };
        let good = plan(&[(0, "第一段。".len(), 0), (second, text.len(), 300)]);
        good.validate(text, &limits).unwrap();
        let (planned, warning) = plan_or_single(&Fixed(Ok(good.clone())), text, limits).unwrap();
        assert_eq!((planned, warning), (good, None));
    }

    #[test]
    fn failed_planner_cannot_fall_back_to_a_reply_that_exceeds_the_part_budget() {
        let text = "第一段。\n\n第二段。";
        let limits = SegmentLimits {
            max_segments: 2,
            max_segment_bytes: "第一段。".len(),
            max_pause_ms: 1000,
        };
        assert_eq!(
            plan_or_single(
                &Fixed(Err(SegmentError::Planner("down".into()))),
                text,
                limits
            ),
            Err(SegmentError::InvalidPlan("单段超过字节预算"))
        );
    }

    #[test]
    fn composition_falls_back_to_whole_reply_on_error_panic_or_invalid_plan() {
        let text = "  第一段。\n\n第二段。 ";
        let whole = SegmentPlan::single(FALLBACK_PLANNER, text).unwrap();
        assert_eq!(
            whole.texts(text).collect::<Vec<_>>(),
            ["第一段。\n\n第二段。"]
        );
        let (planned, warning) = plan_or_single(
            &Fixed(Err(SegmentError::Planner("down".into()))),
            text,
            LIMITS,
        )
        .unwrap();
        assert_eq!((planned, warning.is_some()), (whole.clone(), true));
        let (planned, warning) = plan_or_single(&Panics, text, LIMITS).unwrap();
        assert_eq!(planned, whole);
        assert_eq!(warning, Some(SegmentError::Planner("规划器 panic".into())));
        let (planned, warning) =
            plan_or_single(&Fixed(Ok(plan(&[(2, 5, 0)]))), text, LIMITS).unwrap();
        assert_eq!(planned, whole);
        assert!(matches!(warning, Some(SegmentError::InvalidPlan(_))));
        let first = text.find("第一").unwrap();
        let second = text.find("第二").unwrap();
        let good = plan(&[
            (first, first + "第一段。".len(), 0),
            (second, second + "第二段。".len(), 300),
        ]);
        let (planned, warning) = plan_or_single(&Fixed(Ok(good.clone())), text, LIMITS).unwrap();
        assert_eq!((planned, warning), (good, None));
        assert_eq!(
            plan_or_single(&Panics, " \n ", LIMITS),
            Err(SegmentError::InvalidText)
        );
    }
}
