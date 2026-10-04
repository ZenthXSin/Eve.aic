use super::{error, valid};
use eve_llm_api::ContextScope;
use eve_plugin_api::PluginResult;
use eve_training_api::ExpressionSnapshot;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub(crate) const KEY: &str = "expression.v1";
const MAX_BYTES: usize = 1_048_576;
const MAX_ROWS: usize = 4096;
const MAX_SCOPES: usize = 256;

fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    scope: ContextScope,
    id: String,
    body: String,
    chars: usize,
    paragraphs: usize,
    question: bool,
    formal: bool,
    casual: bool,
    active: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expressions {
    version: u8,
    rows: Vec<Row>,
}
impl Expressions {
    pub(crate) fn load(bytes: Option<Vec<u8>>) -> PluginResult<Self> {
        let Some(bytes) = bytes else {
            return Ok(Self {
                version: 1,
                rows: vec![],
            });
        };
        if bytes.len() > MAX_BYTES {
            return Err(error("表达训练状态损坏；未清空"));
        }
        let value: Self =
            serde_json::from_slice(&bytes).map_err(|_| error("表达训练状态损坏；未清空"))?;
        let mut ids = BTreeSet::new();
        let mut scopes = BTreeSet::new();
        if value.version != 1
            || value.rows.len() > MAX_ROWS
            || value.rows.iter().any(|r| {
                scopes.insert((&r.scope.session_id, &r.scope.user_id));
                !valid(&r.scope)
                    || !valid_hash(&r.id)
                    || !valid_hash(&r.body)
                    || !((1..=512).contains(&r.chars)
                        && r.paragraphs > 0
                        && r.paragraphs <= r.chars
                        || !r.active
                            && r.chars == 0
                            && r.paragraphs == 0
                            && !r.question
                            && !r.formal
                            && !r.casual)
                    || !ids.insert((&r.scope.session_id, &r.scope.user_id, &r.id))
            })
            || scopes.len() > MAX_SCOPES
        {
            return Err(error("表达训练状态损坏或版本不兼容；未清空"));
        }
        Ok(value)
    }
    pub(crate) fn bytes(&self) -> PluginResult<Vec<u8>> {
        let bytes = serde_json::to_vec(self).map_err(|_| error("表达训练编码失败"))?;
        if bytes.len() > MAX_BYTES {
            return Err(error("表达训练容量已满；保留状态"));
        }
        Ok(bytes)
    }
    pub(crate) fn observe(
        &mut self,
        scope: &ContextScope,
        id: &str,
        text: &str,
        enabled: bool,
    ) -> PluginResult<bool> {
        if id.is_empty()
            || id.len() > 256
            || id.trim() != id
            || id.chars().any(char::is_control)
            || text.len() > 32768
        {
            return Err(error("表达训练输入无效"));
        }
        let id = hash(id.as_bytes());
        let body = hash(text.as_bytes());
        if let Some(row) = self.rows.iter().find(|r| r.scope == *scope && r.id == id) {
            return if row.body == body {
                Ok(false)
            } else {
                Err(error("表达训练消息 ID 冲突；保留状态"))
            };
        }
        // 旧 QQ 历史中的开头 @ 仅在统计时剥离；不改写原文或验证路由。
        let mut content = text.trim();
        while content.starts_with("<@") {
            let Some(end) = content.find('>') else { break };
            let marker = content[2..end].trim_start_matches('!');
            if marker.is_empty()
                || !marker
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                break;
            }
            content = content[end + 1..].trim_start();
        }
        let chars = content.chars().count();
        if !(1..=512).contains(&chars)
            || content.starts_with('/')
            || content.contains("```")
            || content.contains("http://")
            || content.contains("https://")
            || content
                .lines()
                .any(|line| line.trim_start().starts_with('>'))
        {
            return Ok(false);
        }
        let scopes: BTreeSet<_> = self
            .rows
            .iter()
            .map(|r| (&r.scope.session_id, &r.scope.user_id))
            .collect();
        if self.rows.len() >= MAX_ROWS
            || (!scopes.contains(&(&scope.session_id, &scope.user_id))
                && scopes.len() >= MAX_SCOPES)
        {
            return Err(error("表达训练容量已满；保留状态"));
        }
        let mut paragraphs = 0;
        let mut empty = true;
        for line in content.lines() {
            if line.trim().is_empty() {
                empty = true;
            } else {
                if empty {
                    paragraphs += 1;
                }
                empty = false;
            }
        }
        self.rows.push(Row {
            scope: scope.clone(),
            id,
            body,
            chars: if enabled { chars } else { 0 },
            paragraphs: if enabled { paragraphs } else { 0 },
            question: enabled && content.contains(['?', '？']),
            formal: enabled
                && ["您好", "请问", "感谢", "敬请"]
                    .iter()
                    .any(|w| content.contains(w)),
            casual: enabled
                && ["哈哈", "嗯", "嘿", "呀", "啦", "呗"]
                    .iter()
                    .any(|w| content.contains(w)),
            active: enabled,
        });
        Ok(true)
    }
    pub(crate) fn reset(&mut self, scope: &ContextScope) -> bool {
        let mut changed = false;
        for row in &mut self.rows {
            if row.scope == *scope && row.active {
                row.active = false;
                changed = true;
            }
        }
        changed
    }
    pub(crate) fn snapshot(&self, scope: Option<&ContextScope>) -> Option<ExpressionSnapshot> {
        let rows: Vec<_> = self
            .rows
            .iter()
            .filter(|r| r.active && scope.is_none_or(|s| r.scope == *s))
            .collect();
        if rows.is_empty() {
            return None;
        }
        let mut lengths: Vec<_> = rows.iter().map(|r| r.chars).collect();
        lengths.sort_unstable();
        let samples = rows.len();
        let percent = |count: usize| count * 100 / samples;
        let revision = hash(&serde_json::to_vec(&rows).expect("表达统计字段均可序列化"));
        Some(ExpressionSnapshot {
            revision,
            samples,
            median_chars: lengths[(samples - 1) / 2],
            p75_chars: lengths[(samples - 1) * 3 / 4],
            short_percent: percent(rows.iter().filter(|r| r.chars <= 60).count()),
            single_paragraph_percent: percent(rows.iter().filter(|r| r.paragraphs == 1).count()),
            question_percent: percent(rows.iter().filter(|r| r.question).count()),
            formal_percent: percent(rows.iter().filter(|r| r.formal).count()),
            casual_percent: percent(rows.iter().filter(|r| r.casual).count()),
        })
    }
}
