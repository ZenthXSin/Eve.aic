//! 规则分段规划：按自然段把同一轮回复分成少量消息，保持代码块、列表、标题和引出句完整，
//! 并按下一段长度给出有上限的停顿建议。只返回原文断点，不访问模型、Session 或通道。
use eve_segment_api::{
    Segment, SegmentError, SegmentPlan, SegmentPlanner, SegmentRequest, SegmentResult,
    fenced_code_spans,
};
use std::ops::Range;

pub const PARAGRAPH_PLANNER: &str = "paragraph-v3";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParagraphPlanner {
    /// 非空白字符少于该值的回复整条发送；默认为 0，短自然段也分别发送。
    pub min_chars: usize,
    pub base_pause_ms: u64,
    pub pause_per_char_ms: u64,
}
impl Default for ParagraphPlanner {
    fn default() -> Self {
        Self {
            min_chars: 0,
            base_pause_ms: 400,
            pause_per_char_ms: 25,
        }
    }
}

/// 自然段过多时规则分段的收益很低；整条发送并保持规划为线性开销。
pub const MAX_UNITS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Prose,
    Heading,
    List,
    Rule,
    Other,
}
struct Block {
    range: Range<usize>,
    kind: Kind,
    intro: bool,
}
struct Line {
    start: usize,
    content: Range<usize>,
    code: bool,
}

fn visible(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}
/// 只有表情、标点或零宽字符的片段不值得单独发送。
fn has_content(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}
fn trimmed(text: &str, range: Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    let start = range.start + slice.len() - slice.trim_start().len();
    start..range.start + slice.trim_end().len()
}
fn indented(line: &str) -> bool {
    line.starts_with([' ', '\t'])
}
/// 去掉行首的强调标记（`**1. 步骤**`），但保留 `* ` 这类列表符号。
fn unemphasized(line: &str) -> &str {
    let body = line.trim_start();
    let stripped = body.trim_start_matches(['*', '_']);
    if stripped.len() < body.len()
        && !stripped.is_empty()
        && !stripped.starts_with(char::is_whitespace)
    {
        stripped
    } else {
        body
    }
}
fn list_item(line: &str) -> bool {
    let body = unemphasized(line);
    if ["- ", "* ", "+ ", "• ", "· "]
        .iter()
        .any(|m| body.starts_with(m))
    {
        return true;
    }
    let mut chars = body.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // ①–⑳、❶–❿
    if ('\u{2460}'..='\u{2473}').contains(&first) || ('\u{2776}'..='\u{277f}').contains(&first) {
        return true;
    }
    let numeral = |c: char| c.is_ascii_digit() || "一二三四五六七八九十".contains(c);
    // （1）、(2)、（三）
    if matches!(first, '(' | '（') {
        let inner: String = chars.clone().take_while(|c| numeral(*c)).collect();
        let count = inner.chars().count();
        return (1..=3).contains(&count)
            && chars.nth(count).is_some_and(|c| matches!(c, ')' | '）'));
    }
    // 1. 1) 1、 一、
    let count = body.chars().take_while(|c| numeral(*c)).count();
    if !(1..=3).contains(&count) {
        return false;
    }
    let mut rest = body.chars().skip(count);
    match rest.next() {
        Some('、') => true,
        Some('.' | ')' | '）' | '．') if body.chars().take(count).all(|c| c.is_ascii_digit()) => {
            rest.next()
                .is_some_and(|c| c.is_whitespace() || !c.is_ascii())
        }
        _ => false,
    }
}
/// 以冒号结尾的引出句（允许末尾强调标记）连接后文。
fn intro(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(['*', '_'])
        .ends_with([':', '：'])
}
fn classify(line: &str) -> Kind {
    let body = line.trim_start();
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|c| compact.chars().all(|x| x == *c))
    {
        Kind::Rule
    } else if body.starts_with('#') {
        Kind::Heading
    } else if list_item(body) {
        Kind::List
    } else if body.starts_with(['>', '|', '`', '~'])
        || line.starts_with('\t')
        || line.starts_with("    ")
    {
        // 引用、表格、行内代码开头与缩进代码不在行间断开。
        Kind::Other
    } else {
        Kind::Prose
    }
}
fn sentence_end(line: &str) -> bool {
    line.trim_end()
        .ends_with(['。', '！', '？', '!', '?', '…', '~', '～', '.'])
}

/// 代码块范围按起点有序且互不重叠，逐行线性扫描。
fn lines(text: &str) -> Vec<Line> {
    let code = fenced_code_spans(text);
    let mut span = 0;
    let mut out = Vec::new();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        while span < code.len() && code[span].end <= offset {
            span += 1;
        }
        out.push(Line {
            start: offset,
            content: offset..offset + body.len(),
            code: span < code.len() && code[span].start <= offset + body.len(),
        });
        offset += raw.len();
    }
    out
}

/// 先按空行分块，空行后的缩进行（缩进代码、列表续段）仍属前一块；
/// 整段没有空行时，只在普通句子之间断开，列表结束后的普通句子也可以断开。
fn blocks(text: &str) -> Vec<Block> {
    let lines = lines(text);
    let content = |l: &Line| &text[l.content.clone()];
    let blank = |l: &Line| !l.code && content(l).trim().is_empty();
    let mut groups: Vec<Vec<&Line>> = Vec::new();
    let mut current: Vec<&Line> = Vec::new();
    let mut after_blank = false;
    for line in &lines {
        if blank(line) {
            after_blank = true;
            continue;
        }
        if after_blank && !current.is_empty() && !(indented(content(line)) && !line.code) {
            groups.push(std::mem::take(&mut current));
        }
        after_blank = false;
        current.push(line);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    if groups.len() == 1 {
        let only = groups.pop().expect("one group");
        // later_list[i]：第 i 行之后是否还有列表项。
        let mut later_list = vec![false; only.len()];
        for i in (0..only.len().saturating_sub(1)).rev() {
            later_list[i] = later_list[i + 1] || classify(content(only[i + 1])) == Kind::List;
        }
        let mut current: Vec<&Line> = Vec::new();
        for (i, line) in only.iter().enumerate() {
            if let Some(previous) = current.last() {
                let before = content(previous);
                let after = content(line);
                let kind = classify(before);
                let in_list = kind == Kind::List || indented(before);
                if !previous.code
                    && !line.code
                    && !indented(after)
                    && classify(after) == Kind::Prose
                    && matches!(kind, Kind::Prose | Kind::List)
                    && sentence_end(before)
                    && !(in_list && later_list[i - 1])
                {
                    groups.push(std::mem::take(&mut current));
                }
            }
            current.push(line);
        }
        groups.push(current);
    }
    groups
        .into_iter()
        .map(|group| {
            let first = group.first().expect("non-empty group");
            let last = group.last().expect("non-empty group");
            let range = trimmed(text, first.start..last.content.end);
            let kind = match first.code {
                true => Kind::Other,
                // 只有单独成块的标题才连接下一块。
                false => match classify(content(first)) {
                    Kind::Heading if group.len() > 1 => Kind::Prose,
                    kind => kind,
                },
            };
            let intro = !last.code && intro(&text[range.clone()]);
            Block { range, kind, intro }
        })
        .filter(|b| !b.range.is_empty())
        .collect()
}

/// 标题、分隔线和以冒号结尾的引出句连接后一块；相邻列表块保持在一起。
/// 分隔线并入前一块时保留其“连接后文”的状态，标题后接分隔线仍与正文同段。
fn units(blocks: Vec<Block>) -> Vec<Range<usize>> {
    let mut units: Vec<Range<usize>> = Vec::new();
    let mut forward = false;
    let mut previous_kind: Option<Kind> = None;
    for block in &blocks {
        let bind = !units.is_empty()
            && (forward
                || block.kind == Kind::Rule
                || (previous_kind == Some(Kind::List) && block.kind == Kind::List));
        match units.last_mut() {
            Some(unit) if bind => unit.end = block.range.end,
            _ => units.push(block.range.clone()),
        }
        forward = match block.kind {
            Kind::Rule => !bind || forward,
            Kind::Heading => true,
            _ => block.intro,
        };
        previous_kind = Some(block.kind);
    }
    units
}

impl SegmentPlanner for ParagraphPlanner {
    fn plan(&self, request: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
        let text = request.text;
        let limits = request.limits;
        let mut units = units(blocks(text));
        // 只有标点、表情或零宽字符的块并入前一段（首块并入后一段）。
        let mut index = 1;
        while index < units.len() {
            if has_content(&text[units[index].clone()]) {
                index += 1;
            } else {
                units[index - 1].end = units[index].end;
                units.remove(index);
            }
        }
        if units.len() > 1 && !has_content(&text[units[0].clone()]) {
            units[1].start = units[0].start;
            units.remove(0);
        }
        if units.len() <= 1 || units.len() > MAX_UNITS || visible(text) < self.min_chars {
            return SegmentPlan::single(PARAGRAPH_PLANNER, text);
        }
        let mut counts: Vec<usize> = units.iter().map(|u| visible(&text[u.clone()])).collect();
        while units.len() > limits.max_segments {
            let fits = |i: &usize| units[*i + 1].end - units[*i].start <= limits.max_segment_bytes;
            let cost = |i: &usize| (counts[*i] + counts[*i + 1], usize::MAX - i);
            // 预算允许三段及以上时保留单独的开头与结尾，先合并中间解释。
            let middle = (1..units.len().saturating_sub(2))
                .filter(|_| limits.max_segments >= 3)
                .filter(fits)
                .min_by_key(cost);
            let best = middle
                .or_else(|| (0..units.len() - 1).filter(fits).min_by_key(cost))
                .ok_or_else(|| SegmentError::Planner("无法在单段预算内合并".into()))?;
            units[best].end = units[best + 1].end;
            units.remove(best + 1);
            counts[best] += counts.remove(best + 1);
        }
        if units
            .iter()
            .any(|u| u.end - u.start > limits.max_segment_bytes)
        {
            return Err(SegmentError::Planner("自然段超过单段预算".into()));
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
                    let chars = u64::try_from(*count).unwrap_or(u64::MAX);
                    self.base_pause_ms
                        .saturating_add(self.pause_per_char_ms.saturating_mul(chars))
                        .min(limits.max_pause_ms)
                },
            })
            .collect();
        Ok(SegmentPlan {
            planner: PARAGRAPH_PLANNER.into(),
            segments,
        })
    }
}
