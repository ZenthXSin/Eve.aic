//! 规则分段规划：按自然段把同一轮回复分成少量消息，保持代码块、列表、标题和引出句完整，
//! 并按下一段长度给出有上限的停顿建议。只返回原文断点，不访问模型、Session 或通道。
use eve_segment_api::{
    Segment, SegmentError, SegmentPlan, SegmentPlanner, SegmentRequest, SegmentResult,
    fenced_code_spans,
};
use std::ops::Range;

pub const PARAGRAPH_PLANNER: &str = "paragraph-v2";

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
fn trimmed(text: &str, range: Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    let start = range.start + slice.len() - slice.trim_start().len();
    start..range.start + slice.trim_end().len()
}
fn list_item(line: &str) -> bool {
    let line = line.trim_start();
    if ["- ", "* ", "+ ", "• ", "· "]
        .iter()
        .any(|m| line.starts_with(m))
    {
        return true;
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    digits > 0
        && line[digits..].starts_with(['.', ')', '、', '）'])
        && line[digits..]
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_whitespace() || !c.is_ascii())
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

fn lines(text: &str) -> Vec<Line> {
    let code = fenced_code_spans(text);
    let mut out = Vec::new();
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let body = raw.trim_end_matches(['\n', '\r']);
        out.push(Line {
            start: offset,
            content: offset..offset + body.len(),
            code: code
                .iter()
                .any(|s| s.start <= offset + body.len() && offset < s.end),
        });
        offset += raw.len();
    }
    out
}

/// 先按空行分块；整段没有空行时，只在两行普通句子之间（前一行以句末标点结束）断开。
fn blocks(text: &str) -> Vec<Block> {
    let lines = lines(text);
    let blank = |l: &Line| !l.code && text[l.content.clone()].trim().is_empty();
    let mut groups: Vec<Vec<&Line>> = Vec::new();
    let mut current: Vec<&Line> = Vec::new();
    for line in &lines {
        if blank(line) {
            if !current.is_empty() {
                groups.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    if groups.len() == 1 {
        let only = groups.pop().expect("one group");
        let mut current: Vec<&Line> = Vec::new();
        for line in only {
            if let Some(previous) = current.last() {
                let before = &text[previous.content.clone()];
                let after = &text[line.content.clone()];
                // 列表结束后的普通句子也可以断开；列表项之间与引出句之后不断开。
                if !previous.code
                    && !line.code
                    && matches!(classify(before), Kind::Prose | Kind::List)
                    && classify(after) == Kind::Prose
                    && sentence_end(before)
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
                false => match classify(&text[first.content.clone()]) {
                    Kind::Heading if group.len() > 1 => Kind::Prose,
                    kind => kind,
                },
            };
            let intro = !last.code && text[range.clone()].ends_with([':', '：']);
            Block { range, kind, intro }
        })
        .filter(|b| !b.range.is_empty())
        .collect()
}

/// 标题、分隔线和以冒号结尾的引出句连接后一块；相邻列表块保持在一起。
fn units(blocks: Vec<Block>) -> Vec<Range<usize>> {
    let mut units: Vec<Range<usize>> = Vec::new();
    let mut previous: Option<&Block> = None;
    for block in &blocks {
        let bind = match previous {
            None => false,
            Some(p) => {
                p.intro
                    || matches!(p.kind, Kind::Heading)
                    || (p.kind == Kind::Rule && units.last().is_some_and(|u| *u == p.range))
                    || block.kind == Kind::Rule
                    || (p.kind == Kind::List && block.kind == Kind::List)
            }
        };
        match units.last_mut() {
            Some(unit) if bind => unit.end = block.range.end,
            _ => units.push(block.range.clone()),
        }
        previous = Some(block);
    }
    units
}

impl SegmentPlanner for ParagraphPlanner {
    fn plan(&self, request: &SegmentRequest<'_>) -> SegmentResult<SegmentPlan> {
        let text = request.text;
        let limits = request.limits;
        let mut units = units(blocks(text));
        // 只有标点或表情的极短块并入前一段，避免单独发出无内容的消息。
        let mut index = 1;
        while index < units.len() {
            if visible(&text[units[index].clone()]) < 2 {
                units[index - 1].end = units[index].end;
                units.remove(index);
            } else {
                index += 1;
            }
        }
        if units.len() > 1 && visible(&text[units[0].clone()]) < 2 {
            units[1].start = units[0].start;
            units.remove(0);
        }
        if units.len() <= 1 || visible(text) < self.min_chars {
            return SegmentPlan::single(PARAGRAPH_PLANNER, text);
        }
        while units.len() > limits.max_segments {
            let fits = |i: &usize| units[*i + 1].end - units[*i].start <= limits.max_segment_bytes;
            let cost = |i: &usize| {
                (
                    visible(&text[units[*i].start..units[*i + 1].end]),
                    usize::MAX - i,
                )
            };
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
        }
        if units
            .iter()
            .any(|u| u.end - u.start > limits.max_segment_bytes)
        {
            return Err(SegmentError::Planner("自然段超过单段预算".into()));
        }
        let segments = units
            .iter()
            .enumerate()
            .map(|(index, unit)| Segment {
                start: unit.start,
                end: unit.end,
                pause_before_ms: if index == 0 {
                    0
                } else {
                    let chars = u64::try_from(visible(&text[unit.clone()])).unwrap_or(u64::MAX);
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
