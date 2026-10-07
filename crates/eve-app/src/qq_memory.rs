//! QQ 用户通过显式命令管理偏好和检索历史来源；不推断偏好或调用模型。
use eve_memory_api::{
    DEFAULT_RECALL_RESULTS, MAX_PREFERENCE_BYTES, MAX_RECALL_QUERY_BYTES,
    MAX_RECALL_RESPONSE_BYTES, MAX_TEXT_BYTES, MemoryAdmin, MemoryError, MemoryRecallFactory,
    MemoryRecallRequest, MemoryRecallResponse, MemoryRecallSource, MemoryScope, MemorySnapshot,
    PreferenceAction, PreferenceChange, PreferenceEvidence, PreferenceStatus, RecallField,
    UserStatement, validate_id, validate_text,
};
use eve_memory_plugin::LexicalMemoryRecall;
use eve_plugin_api::{PluginError, PluginResult};
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use eve_session_api::SessionKey;
use ring::digest::{Context, SHA256};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

const HELP: &str = "用法：/remember 偏好内容、/memories [页码]、/recall 关键词、/correct-memory 偏好ID 新内容、/forget 偏好ID。";
const PAGE_SIZE: usize = 10;
const PREVIEW_BYTES: usize = 512;

/// 与通道生成的完整会话标识绑定，不从命令正文读取身份。
pub(crate) fn scope(session: &SessionKey) -> MemoryScope {
    MemoryScope {
        channel: "qq".into(),
        session_id: session.session_id.clone(),
        user_id: session.user_id.clone(),
    }
}

fn message_digest(input: &QqCommandInput<'_>) -> String {
    let mut hash = Context::new(&SHA256);
    for part in [
        "qq".as_bytes(),
        input.session.session_id.as_bytes(),
        input.session.user_id.as_bytes(),
        input.message_id.as_bytes(),
    ] {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn failure() -> PluginError {
    // 持久化失败可能已提交，不承诺状态未变，也不自动再次执行命令。
    PluginError::State("记忆状态无法确认；服务已停止，请重新打开后查看持久状态。".into())
}

fn recall_failure() -> PluginError {
    PluginError::State("记忆检索结果无法确认；请重新打开服务后重试。".into())
}

fn explain(error: MemoryError) -> PluginResult<String> {
    match error {
        MemoryError::InvalidInput => Ok(format!(
            "偏好内容须为 1 至 {MAX_PREFERENCE_BYTES} UTF-8 字节，且不能包含空字符；ID 必须有效。"
        )),
        MemoryError::NotFound => Ok("当前会话没有这条偏好。发送 /memories 查看偏好 ID。".into()),
        MemoryError::Conflict => Ok(
            "偏好操作与已有状态或消息记录冲突；请先查看 /memories，再使用新消息提交命令。".into(),
        ),
        MemoryError::StaleRevision => Ok("记忆刚刚发生变化；请重新查看后发送新消息重试。".into()),
        MemoryError::LimitReached => Ok("记忆容量已满；原记录已保留，本次没有新增记录。".into()),
        MemoryError::Unavailable
        | MemoryError::CorruptState
        | MemoryError::UnsupportedVersion
        | MemoryError::Storage => Err(failure()),
    }
}

pub(crate) struct Commands {
    admin: Option<Arc<dyn MemoryAdmin>>,
    recall: Option<Arc<dyn MemoryRecallFactory>>,
}
impl Commands {
    pub(crate) fn new(admin: Arc<dyn MemoryAdmin>) -> Arc<Self> {
        let recall = Arc::new(LexicalMemoryRecall::new(admin.clone()));
        Self::with_recall(admin, recall)
    }

    /// 组合层可替换检索实现；命令始终校验返回范围和公开结果契约。
    pub(crate) fn with_recall(
        admin: Arc<dyn MemoryAdmin>,
        recall: Arc<dyn MemoryRecallFactory>,
    ) -> Arc<Self> {
        Arc::new(Self {
            admin: Some(admin),
            recall: Some(recall),
        })
    }

    pub(crate) fn disabled() -> Arc<Self> {
        Arc::new(Self {
            admin: None,
            recall: None,
        })
    }

    fn execute(&self, input: &QqCommandInput<'_>, command: Command<'_>) -> PluginResult<String> {
        let Some(admin) = &self.admin else {
            return Ok("交互记忆未启用。".into());
        };
        if command == Command::Help {
            return Ok(HELP.into());
        }
        if input.session.validate().is_err() || validate_id(input.message_id).is_err() {
            return Err(PluginError::Task("QQ 记忆命令输入无效。".into()));
        }
        let scope = scope(input.session);
        if let Command::Recall(query) = command {
            let request = MemoryRecallRequest {
                query: query.into(),
                limit: DEFAULT_RECALL_RESULTS,
            };
            if request.validate().is_err() {
                return Ok(format!(
                    "用法：/recall 关键词。关键词须为 1 至 {MAX_RECALL_QUERY_BYTES} UTF-8 字节，且不能包含控制字符；每次最多显示 {DEFAULT_RECALL_RESULTS} 条。"
                ));
            }
            let result = self
                .recall
                .as_ref()
                .ok_or_else(recall_failure)?
                .reader(scope.clone())
                .and_then(|reader| reader.recall(&request))
                .map_err(|_| recall_failure())?;
            result
                .validate_for(&scope, &request)
                .map_err(|_| recall_failure())?;
            return recall_list(&result);
        }
        if let Err(error) = validate_text(input.text, MAX_TEXT_BYTES) {
            return explain(error);
        }
        let snapshot = match admin
            .reader(scope.clone())
            .and_then(|reader| reader.snapshot())
        {
            Ok(snapshot) if snapshot.scope == scope => snapshot,
            Ok(_) => return Err(failure()),
            Err(error) => return explain(error),
        };
        if let Command::Memories(page) = command {
            return Ok(list(&snapshot, page));
        }
        let digest = message_digest(input);
        let (action, reply) = match command {
            Command::Remember(text) => {
                if let Err(error) = validate_text(text, MAX_PREFERENCE_BYTES) {
                    return explain(error);
                }
                let id = format!("qq-memory-{digest}");
                let reply =
                    format!("偏好已保存：{id}。发送 /memories 查看，或 /forget {id} 撤销。");
                (
                    PreferenceAction::Confirm {
                        id,
                        text: text.into(),
                    },
                    reply,
                )
            }
            Command::Correct { id, text } => {
                if let Err(error) =
                    validate_id(id).and_then(|()| validate_text(text, MAX_PREFERENCE_BYTES))
                {
                    return explain(error);
                }
                (
                    PreferenceAction::Correct {
                        id: id.into(),
                        text: text.into(),
                    },
                    format!("偏好已修正：{id}。"),
                )
            }
            Command::Forget(id) => {
                if let Err(error) = validate_id(id) {
                    return explain(error);
                }
                (
                    PreferenceAction::Revoke { id: id.into() },
                    format!("偏好已撤销：{id}。历史来源仍保留，该偏好不再作为当前偏好使用。"),
                )
            }
            Command::Help | Command::Memories(_) | Command::Recall(_) => unreachable!(),
        };
        let (preference_id, expected_text) = match &action {
            PreferenceAction::Confirm { id, text } | PreferenceAction::Correct { id, text } => {
                (id.clone(), Some(text.clone()))
            }
            PreferenceAction::Revoke { id } => (id.clone(), None),
        };
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .ok_or_else(failure)?;
        let change = PreferenceChange {
            operation_id: format!("qq-memory-operation-{digest}"),
            at_ms,
            evidence: PreferenceEvidence::Statement(UserStatement {
                evidence_id: format!("qq-memory-evidence-{digest}"),
                message_id: input.message_id.into(),
                // 原始命令是证据，列表摘要和正文规范化不得修改它。
                text: input.text.into(),
                at_ms,
            }),
            action,
        };
        // 同一消息的重放仍提交相同操作 ID，由管理契约先于 CAS 做幂等检查。
        match admin.update_preference(&scope, snapshot.revision, change) {
            Ok(saved) if saved.scope == scope => {
                let current = saved
                    .preferences
                    .iter()
                    .find(|preference| preference.id == preference_id)
                    .ok_or_else(failure)?;
                // 旧消息重放不能把后来的修正或撤销描述成已被恢复。
                if expected_text.is_some() && current.status == PreferenceStatus::Revoked {
                    Ok(format!(
                        "该命令已处理，但偏好目前已撤销：{preference_id}。发送 /memories 查看当前记录。"
                    ))
                } else if expected_text
                    .as_ref()
                    .is_some_and(|text| text != &current.text)
                {
                    Ok(format!(
                        "该命令已处理；偏好已有后续变更：{preference_id}。发送 /memories 查看当前记录。"
                    ))
                } else if expected_text.is_none() && current.status != PreferenceStatus::Revoked {
                    Err(failure())
                } else {
                    Ok(reply)
                }
            }
            Ok(_) => Err(failure()),
            Err(error) => explain(error),
        }
    }
}
impl QqCommandHandler for Commands {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let Some(command) = parse(input.text) else {
            return Ok(None);
        };
        self.execute(&input, command).map(Some)
    }
}

fn recall_list(result: &MemoryRecallResponse) -> PluginResult<String> {
    let mut reply = format!(
        "当前会话记忆检索（memory revision={}，最多 {DEFAULT_RECALL_RESULTS} 条）：",
        result.revision
    );
    if result.hits.is_empty() {
        reply.push_str("\n未找到匹配记忆。");
    }
    for (index, hit) in result.hits.iter().enumerate() {
        match &hit.source {
            MemoryRecallSource::ConfirmedPreference {
                preference_id,
                preference_revision,
                evidence_id,
                evidence_revision,
                ..
            } => reply.push_str(&format!(
                "\n{}. [已确认偏好] preference={} version={} evidence={} evidence_revision={}",
                index + 1,
                preview(preference_id),
                preference_revision,
                preview(evidence_id),
                evidence_revision
            )),
            MemoryRecallSource::CompletedInteraction {
                evidence_id,
                evidence_revision,
                message_id,
                session_revision,
                turn_id,
                field,
                ..
            } => {
                let label = match field {
                    RecallField::User => "用户原话",
                    RecallField::Assistant => "历史助手回复",
                };
                reply.push_str(&format!(
                    "\n{}. [{}] evidence={} evidence_revision={} message={} session_revision={} turn={}",
                    index + 1,
                    label,
                    preview(evidence_id),
                    evidence_revision,
                    preview(message_id),
                    session_revision,
                    turn_id
                ));
            }
        }
        reply.push_str(&format!("\n片段：{}", preview(&hit.excerpt)));
        if hit.excerpt_truncated {
            reply.push_str("（原文有截断）");
        }
    }
    reply.push_str("\n边界：历史助手回复不是偏好或当前事实；片段只表示已保存的历史来源。");
    if reply.len() > MAX_RECALL_RESPONSE_BYTES {
        return Err(recall_failure());
    }
    Ok(reply)
}

fn list(snapshot: &MemorySnapshot, page: usize) -> String {
    if snapshot.preferences.is_empty() {
        return "当前会话没有偏好。发送 /remember 偏好内容 保存。".into();
    }
    let pages = snapshot.preferences.len().div_ceil(PAGE_SIZE);
    if page > pages {
        return format!("当前偏好共有 {pages} 页；发送 /memories 1 查看第一页。");
    }
    let mut preferences: Vec<_> = snapshot.preferences.iter().collect();
    preferences.sort_by(|left, right| left.id.cmp(&right.id));
    let mut reply = format!("当前会话偏好（第 {page}/{pages} 页）：");
    // page 已验证不超过有限快照页数，乘法不会因任意用户页码溢出。
    for preference in preferences
        .into_iter()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
    {
        let status = match preference.status {
            PreferenceStatus::Confirmed => "已确认",
            PreferenceStatus::Revoked => "已撤销",
        };
        reply.push_str(&format!(
            "\n{} [{}] {}",
            preference.id,
            status,
            preview(&preference.text)
        ));
    }
    reply
}

#[derive(Debug, Eq, PartialEq)]
enum Command<'a> {
    Remember(&'a str),
    Memories(usize),
    Recall(&'a str),
    Correct { id: &'a str, text: &'a str },
    Forget(&'a str),
    Help,
}

fn parse(text: &str) -> Option<Command<'_>> {
    let text = text.trim_start();
    let end = text
        .find(|character: char| character.is_whitespace() || character.is_control())
        .unwrap_or(text.len());
    let (name, tail) = text.split_at(end);
    if name == "/recall" {
        // 空格只作命令分隔；查询里的换行/制表符保留给公开契约拒绝，不能静默删除。
        let query = tail.trim_matches(' ');
        return Some(if query.is_empty() {
            Command::Help
        } else {
            Command::Recall(query)
        });
    }
    let tail = tail.trim();
    Some(match name {
        "/remember" if !tail.is_empty() => Command::Remember(tail),
        "/memories" if tail.is_empty() => Command::Memories(1),
        "/memories" => tail
            .parse::<usize>()
            .ok()
            .filter(|page| *page > 0)
            .map(Command::Memories)
            .unwrap_or(Command::Help),
        "/correct-memory" => match tail.find(char::is_whitespace) {
            Some(end) if !tail[end..].trim().is_empty() => Command::Correct {
                id: &tail[..end],
                text: tail[end..].trim(),
            },
            _ => Command::Help,
        },
        "/forget" if !tail.is_empty() && !tail.chars().any(char::is_whitespace) => {
            Command::Forget(tail)
        }
        "/remember" | "/forget" => Command::Help,
        _ => return None,
    })
}

/// 列表只回显一个有界摘要；多行偏好仍在原记录中完整保存。
fn preview(text: &str) -> String {
    let normalized: String = text
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
            {
                ' '
            } else {
                character
            }
        })
        .collect();
    if normalized.len() <= PREVIEW_BYTES {
        return normalized;
    }
    let mut end = PREVIEW_BYTES - '…'.len_utf8();
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &normalized[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tokens_are_exact_and_body_keeps_nested_command_text() {
        for ordinary in [
            "remember 简短一点",
            "/remembering 简短一点",
            "/memories-extra",
            "正文 /forget id",
            "/train status",
        ] {
            assert_eq!(parse(ordinary), None);
        }
        assert_eq!(
            parse("  /remember\t先说结论\n/forget 是正文  "),
            Some(Command::Remember("先说结论\n/forget 是正文"))
        );
        assert_eq!(
            parse("/correct-memory id-1\t新偏好\n还有一点"),
            Some(Command::Correct {
                id: "id-1",
                text: "新偏好\n还有一点"
            })
        );
        assert_eq!(parse("/forget id-1"), Some(Command::Forget("id-1")));
        assert_eq!(
            parse(" /recall 用户原话 "),
            Some(Command::Recall("用户原话"))
        );
    }

    #[test]
    fn invalid_explicit_commands_stay_in_command_path() {
        for text in [
            "/remember",
            "/remember \n ",
            "/correct-memory",
            "/correct-memory id-1",
            "/forget",
            "/forget id-1 extra",
            "/memories 0",
            "/memories -1",
            "/memories text",
            "/memories 9999999999999999999999999999999999999999",
            "/recall",
            "/recall   ",
        ] {
            assert_eq!(parse(text), Some(Command::Help), "{text}");
        }
        assert_eq!(parse(" /memories "), Some(Command::Memories(1)));
        assert_eq!(parse("/memories 2"), Some(Command::Memories(2)));
    }

    #[test]
    fn preview_preserves_utf8_without_emitting_extra_list_lines() {
        assert_eq!(preview("第一行\n第二行\t第三行"), "第一行 第二行 第三行");
        assert_eq!(preview("来源\u{202e}反向\u{2066}伪装"), "来源 反向 伪装");
        let value = preview(&"好".repeat(PREVIEW_BYTES));
        assert!(value.ends_with('…'));
        assert!(value.len() <= PREVIEW_BYTES);
        assert_eq!(
            preview(&"a".repeat(PREVIEW_BYTES)),
            "a".repeat(PREVIEW_BYTES)
        );
    }

    use eve_memory_api::{
        CompletedInteraction, EvidenceSource, InteractionEvidence, MemoryRecallHit,
        MemoryRecallService, MemoryResult, MemoryService, Preference, PreferenceVersion,
    };
    use std::{collections::BTreeMap, sync::Mutex};

    #[derive(Default)]
    struct MockState {
        snapshots: BTreeMap<MemoryScope, MemorySnapshot>,
        operations: BTreeMap<(MemoryScope, String), PreferenceChange>,
        calls: Vec<(MemoryScope, u64, PreferenceChange)>,
        reads: usize,
        writes: usize,
        read_error: Option<MemoryError>,
        write_error: Option<MemoryError>,
        foreign_snapshot: bool,
    }
    #[derive(Clone, Default)]
    struct MockAdmin(Arc<Mutex<MockState>>);
    struct MockReader(MockAdmin, MemoryScope);
    impl MemoryService for MockReader {
        fn snapshot(&self) -> MemoryResult<MemorySnapshot> {
            let state = self.0.0.lock().unwrap();
            if let Some(error) = &state.read_error {
                return Err(error.clone());
            }
            let mut snapshot = state
                .snapshots
                .get(&self.1)
                .cloned()
                .unwrap_or(MemorySnapshot {
                    scope: self.1.clone(),
                    revision: 0,
                    evidence: vec![],
                    preferences: vec![],
                });
            if state.foreign_snapshot {
                snapshot.scope.user_id = "foreign-user".into();
            }
            Ok(snapshot)
        }
    }
    impl MemoryAdmin for MockAdmin {
        fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryService>> {
            self.0.lock().unwrap().reads += 1;
            Ok(Arc::new(MockReader(self.clone(), scope)))
        }
        fn import_completed(
            &self,
            _: &MemoryScope,
            _: u64,
            _: CompletedInteraction,
        ) -> MemoryResult<MemorySnapshot> {
            panic!("显式偏好命令不能伪造完成交互")
        }
        fn update_preference(
            &self,
            scope: &MemoryScope,
            expected_revision: u64,
            change: PreferenceChange,
        ) -> MemoryResult<MemorySnapshot> {
            let mut state = self.0.lock().unwrap();
            state
                .calls
                .push((scope.clone(), expected_revision, change.clone()));
            if let Some(error) = &state.write_error {
                return Err(error.clone());
            }
            let mut snapshot = state
                .snapshots
                .get(scope)
                .cloned()
                .unwrap_or(MemorySnapshot {
                    scope: scope.clone(),
                    revision: 0,
                    evidence: vec![],
                    preferences: vec![],
                });
            let key = (scope.clone(), change.operation_id.clone());
            if let Some(previous) = state.operations.get(&key) {
                let same_evidence = match (&previous.evidence, &change.evidence) {
                    (PreferenceEvidence::Statement(left), PreferenceEvidence::Statement(right)) => {
                        left.evidence_id == right.evidence_id
                            && left.message_id == right.message_id
                            && left.text == right.text
                    }
                    _ => false,
                };
                return if previous.action == change.action && same_evidence {
                    Ok(snapshot)
                } else {
                    Err(MemoryError::Conflict)
                };
            }
            if snapshot.revision != expected_revision {
                return Err(MemoryError::StaleRevision);
            }
            let PreferenceEvidence::Statement(statement) = &change.evidence else {
                panic!("命令必须携带原始用户证据")
            };
            let (id, text, status) = match &change.action {
                PreferenceAction::Confirm { id, text } | PreferenceAction::Correct { id, text } => {
                    (id.clone(), text.clone(), PreferenceStatus::Confirmed)
                }
                PreferenceAction::Revoke { id } => {
                    let old = snapshot
                        .preferences
                        .iter()
                        .find(|item| &item.id == id)
                        .ok_or(MemoryError::NotFound)?;
                    (id.clone(), old.text.clone(), PreferenceStatus::Revoked)
                }
            };
            let existing = snapshot.preferences.iter_mut().find(|item| item.id == id);
            if matches!(change.action, PreferenceAction::Correct { .. }) && existing.is_none() {
                return Err(MemoryError::NotFound);
            }
            let revision = existing.as_ref().map_or(1, |item| item.revision + 1);
            let version = PreferenceVersion {
                revision,
                evidence_id: statement.evidence_id.clone(),
                at_ms: change.at_ms,
                text: text.clone(),
                status: status.clone(),
            };
            match existing {
                Some(preference) => {
                    preference.text = text;
                    preference.status = status;
                    preference.revision = revision;
                    preference.history.push(version);
                }
                None => snapshot.preferences.push(Preference {
                    id,
                    text,
                    status,
                    revision,
                    history: vec![version],
                }),
            }
            snapshot.revision += 1;
            snapshot.evidence.push(InteractionEvidence {
                id: statement.evidence_id.clone(),
                revision: snapshot.revision,
                at_ms: statement.at_ms,
                source: EvidenceSource::UserStatement {
                    message_id: statement.message_id.clone(),
                    text: statement.text.clone(),
                },
            });
            state.operations.insert(key, change);
            state.snapshots.insert(scope.clone(), snapshot.clone());
            state.writes += 1;
            Ok(snapshot)
        }
    }

    fn session() -> SessionKey {
        SessionKey::new(
            "qq-app-group-and-sender-hash",
            "qq-app-group-and-sender-hash",
        )
        .unwrap()
    }
    fn run(
        commands: &Commands,
        session: &SessionKey,
        message_id: &str,
        text: &str,
    ) -> PluginResult<Option<String>> {
        commands.handle(QqCommandInput {
            message_id,
            session,
            text,
        })
    }
    fn setup() -> (MockAdmin, Arc<Commands>, SessionKey) {
        let admin = MockAdmin::default();
        let commands = Commands::new(Arc::new(admin.clone()));
        (admin, commands, session())
    }

    struct RecallMockState {
        scopes: Vec<MemoryScope>,
        queries: Vec<MemoryRecallRequest>,
        result: MemoryResult<MemoryRecallResponse>,
    }

    #[derive(Clone)]
    struct RecallMock(Arc<Mutex<RecallMockState>>);

    impl MemoryRecallFactory for RecallMock {
        fn reader(&self, scope: MemoryScope) -> MemoryResult<Arc<dyn MemoryRecallService>> {
            self.0.lock().unwrap().scopes.push(scope);
            Ok(Arc::new(self.clone()))
        }
    }

    impl MemoryRecallService for RecallMock {
        fn recall(&self, request: &MemoryRecallRequest) -> MemoryResult<MemoryRecallResponse> {
            let mut state = self.0.lock().unwrap();
            state.queries.push(request.clone());
            state.result.clone()
        }
    }

    fn recall_setup(
        result: MemoryResult<MemoryRecallResponse>,
    ) -> (MockAdmin, RecallMock, Arc<Commands>) {
        let admin = MockAdmin::default();
        let recall = RecallMock(Arc::new(Mutex::new(RecallMockState {
            scopes: vec![],
            queries: vec![],
            result,
        })));
        let commands = Commands::with_recall(Arc::new(admin.clone()), Arc::new(recall.clone()));
        (admin, recall, commands)
    }

    fn recall_result() -> MemoryRecallResponse {
        MemoryRecallResponse {
            scope: scope(&session()),
            revision: 3,
            hits: vec![
                MemoryRecallHit {
                    score: 3,
                    source: MemoryRecallSource::ConfirmedPreference {
                        preference_id: "preference-1".into(),
                        preference_revision: 2,
                        evidence_id: "statement-2".into(),
                        evidence_revision: 2,
                        at_ms: 10,
                    },
                    excerpt: "当前偏好\n不要把下一行当来源".into(),
                    excerpt_truncated: true,
                },
                MemoryRecallHit {
                    score: 2,
                    source: MemoryRecallSource::CompletedInteraction {
                        evidence_id: "interaction-3".into(),
                        evidence_revision: 3,
                        message_id: "message-3".into(),
                        session_revision: 2,
                        turn_id: 1,
                        at_ms: 11,
                        field: RecallField::User,
                    },
                    excerpt: "用户原话\t只做记录".into(),
                    excerpt_truncated: false,
                },
                MemoryRecallHit {
                    score: 1,
                    source: MemoryRecallSource::CompletedInteraction {
                        evidence_id: "interaction-3".into(),
                        evidence_revision: 3,
                        message_id: "message-3".into(),
                        session_revision: 2,
                        turn_id: 1,
                        at_ms: 11,
                        field: RecallField::Assistant,
                    },
                    excerpt: "以前的回答\u{2028}尚未核验".into(),
                    excerpt_truncated: false,
                },
            ],
        }
    }

    #[test]
    fn recall_uses_only_bound_read_service_and_preserves_source_kinds() {
        let (admin, recall, commands) = recall_setup(Ok(recall_result()));
        let reply = run(
            &commands,
            &session(),
            "recall-message",
            "/recall preference session_id=foreign",
        )
        .unwrap()
        .unwrap();
        let state = recall.0.lock().unwrap();
        assert_eq!(state.scopes, vec![scope(&session())]);
        assert_eq!(
            state.queries,
            vec![MemoryRecallRequest {
                query: "preference session_id=foreign".into(),
                limit: 5,
            }]
        );
        let admin_state = admin.0.lock().unwrap();
        assert_eq!(admin_state.reads, 0);
        assert_eq!(admin_state.writes, 0);
        assert!(admin_state.calls.is_empty());
        assert!(reply.contains("memory revision=3"));
        assert!(reply.contains("[已确认偏好] preference=preference-1 version=2"));
        assert!(reply.contains("evidence=statement-2 evidence_revision=2"));
        assert!(reply.contains("[用户原话] evidence=interaction-3"));
        assert!(reply.contains("[历史助手回复] evidence=interaction-3"));
        assert!(reply.contains("session_revision=2 turn=1"));
        assert!(reply.contains("历史助手回复不是偏好或当前事实"));
        assert!(reply.contains("片段：当前偏好 不要把下一行当来源（原文有截断）"));
        assert!(reply.contains("片段：以前的回答 尚未核验"));
        assert!(reply.len() <= MAX_RECALL_RESPONSE_BYTES);
    }

    #[test]
    fn recall_invalid_queries_stay_local_and_never_read_or_write() {
        let (admin, recall, commands) = recall_setup(Ok(recall_result()));
        for input in [
            "/recall".to_owned(),
            "/recall    ".to_owned(),
            "/recall 查询\n正文".to_owned(),
            "/recall 查询\n".to_owned(),
            "/recall\t查询".to_owned(),
            "/recall 查询\0正文".to_owned(),
            "/recall\0查询".to_owned(),
            format!("/recall {}", "好".repeat(MAX_RECALL_QUERY_BYTES)),
        ] {
            let reply = run(&commands, &session(), "query-message", &input)
                .unwrap()
                .unwrap();
            assert!(reply.contains("用法："));
            assert!(reply.contains("/recall 关键词"));
        }
        assert!(recall.0.lock().unwrap().scopes.is_empty());
        assert!(recall.0.lock().unwrap().queries.is_empty());
        let state = admin.0.lock().unwrap();
        assert_eq!(state.reads, 0);
        assert_eq!(state.writes, 0);
        assert!(state.calls.is_empty());
    }

    #[test]
    fn recall_rejects_foreign_invalid_and_unavailable_service_results() {
        let original = recall_result();
        let mut foreign = original.clone();
        foreign.scope.session_id = "foreign-session".into();
        let mut invalid_revision = original.clone();
        invalid_revision.revision = 1;
        let mut oversized_excerpt = original.clone();
        oversized_excerpt.hits[0].excerpt = "a".repeat(PREVIEW_BYTES + 1);
        let mut duplicate = original.clone();
        duplicate.hits.push(original.hits[0].clone());
        for result in [
            Ok(foreign),
            Ok(invalid_revision),
            Ok(oversized_excerpt),
            Ok(duplicate),
            Err(MemoryError::Storage),
            Err(MemoryError::Unavailable),
            Err(MemoryError::InvalidInput),
        ] {
            let (admin, _, commands) = recall_setup(result);
            let outcome = run(&commands, &session(), "query-message", "/recall 关键词");
            let Err(PluginError::State(message)) = outcome else {
                panic!("无法确认的召回结果必须拒绝")
            };
            assert!(message.contains("检索结果无法确认"));
            assert!(!message.contains("当前偏好"));
            assert!(!message.contains("以前的回答"));
            let state = admin.0.lock().unwrap();
            assert_eq!(state.reads, 0);
            assert_eq!(state.writes, 0);
        }
    }

    #[test]
    fn recall_empty_result_keeps_snapshot_revision_and_source_boundary() {
        let (admin, _, commands) = recall_setup(Ok(MemoryRecallResponse {
            scope: scope(&session()),
            revision: 19,
            hits: vec![],
        }));
        let reply = run(&commands, &session(), "query-message", "/recall 没有的内容")
            .unwrap()
            .unwrap();
        assert!(reply.contains("memory revision=19"));
        assert!(reply.contains("未找到匹配记忆"));
        assert!(reply.contains("历史助手回复不是偏好或当前事实"));
        assert_eq!(admin.0.lock().unwrap().reads, 0);
    }

    #[test]
    fn disabled_recognized_commands_never_fall_back_to_chat() {
        let commands = Commands::disabled();
        for text in [
            "/remember 新偏好",
            "/remember",
            "/memories",
            "/recall 关键词",
            "/recall",
            "/correct-memory id 新值",
            "/forget id",
        ] {
            assert!(
                run(&commands, &session(), "message-1", text)
                    .unwrap()
                    .unwrap()
                    .contains("未启用")
            );
        }
        assert!(
            run(&commands, &session(), "message-1", "普通聊天")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn save_is_one_cas_with_full_original_command_and_opaque_message_id() {
        let (admin, commands, session) = setup();
        let message_id = "A".repeat(253);
        let raw = "  /remember\t先说结论\n再给细节  ";
        let reply = run(&commands, &session, &message_id, raw).unwrap().unwrap();
        let state = admin.0.lock().unwrap();
        assert_eq!(state.calls.len(), 1);
        assert_eq!(state.writes, 1);
        let (saved_scope, expected_revision, change) = &state.calls[0];
        assert_eq!(saved_scope, &scope(&session));
        assert_eq!(*expected_revision, 0);
        let PreferenceEvidence::Statement(statement) = &change.evidence else {
            panic!()
        };
        assert_eq!(statement.text, raw);
        assert_eq!(statement.message_id, message_id);
        assert_eq!(statement.at_ms, change.at_ms);
        assert!(statement.evidence_id.len() <= 256);
        assert!(change.operation_id.len() <= 256);
        let PreferenceAction::Confirm { id, text } = &change.action else {
            panic!()
        };
        assert_eq!(text, "先说结论\n再给细节");
        assert!(id.len() <= 256);
        assert!(reply.contains(id));
        let saved = &state.snapshots[&scope(&session)];
        assert_eq!(saved.evidence.len(), 1);
        assert_eq!(
            saved.preferences[0].history[0].evidence_id,
            statement.evidence_id
        );
    }

    #[test]
    fn replay_uses_same_operation_and_does_not_rewrite_after_other_commands() {
        let (admin, commands, session) = setup();
        let first = run(&commands, &session, "first", "/remember 简短回复").unwrap();
        run(&commands, &session, "second", "/remember 使用中文").unwrap();
        let repeated = run(&commands, &session, "first", "/remember 简短回复").unwrap();
        assert_eq!(first, repeated);
        let state = admin.0.lock().unwrap();
        assert_eq!(state.writes, 2);
        assert_eq!(state.calls.len(), 3);
        assert_eq!(state.calls[0].2.operation_id, state.calls[2].2.operation_id);
        assert_eq!(state.calls[2].1, 2);
        assert_eq!(state.snapshots[&scope(&session)].revision, 2);
        drop(state);
        let conflict = run(&commands, &session, "first", "/remember 篡改正文")
            .unwrap()
            .unwrap();
        assert!(conflict.contains("冲突"));
        assert_eq!(admin.0.lock().unwrap().writes, 2);
    }

    #[test]
    fn digest_is_stable_unambiguous_and_keeps_full_host_scope() {
        fn input<'a>(session: &'a SessionKey, message_id: &'a str) -> QqCommandInput<'a> {
            QqCommandInput {
                message_id,
                session,
                text: "/remember 偏好",
            }
        }
        let a = SessionKey::new("ab", "c").unwrap();
        let b = SessionKey::new("a", "bc").unwrap();
        let c = SessionKey::new("ab", "different-user").unwrap();
        let d = SessionKey::new("other-app-group", "c").unwrap();
        let a_digest = message_digest(&input(&a, "message"));
        assert_eq!(a_digest, message_digest(&input(&a, "message")));
        for session in [&b, &c, &d] {
            assert_ne!(a_digest, message_digest(&input(session, "message")));
        }
        assert_ne!(a_digest, message_digest(&input(&a, "message-2")));
        let mut long_id = "a".repeat(253);
        let original = message_digest(&input(&a, &long_id));
        long_id.replace_range(252.., "b");
        assert_ne!(original, message_digest(&input(&a, &long_id)));
    }

    #[test]
    fn correction_and_revocation_preserve_evidence_and_stay_in_own_scope() {
        let (admin, commands, session) = setup();
        run(&commands, &session, "remember", "/remember 原偏好").unwrap();
        let id = admin.0.lock().unwrap().snapshots[&scope(&session)].preferences[0]
            .id
            .clone();
        let foreign = SessionKey::new("other-qq-app-group", session.user_id.clone()).unwrap();
        assert!(
            run(
                &commands,
                &foreign,
                "foreign-correction",
                &format!("/correct-memory {id} 不允许跨会话")
            )
            .unwrap()
            .unwrap()
            .contains("没有")
        );
        assert!(
            run(
                &commands,
                &foreign,
                "foreign-forget",
                &format!("/forget {id}")
            )
            .unwrap()
            .unwrap()
            .contains("没有")
        );
        assert!(
            !run(&commands, &foreign, "foreign-list", "/memories")
                .unwrap()
                .unwrap()
                .contains("原偏好")
        );
        let correction = format!("/correct-memory {id} 新偏好\n第二行");
        run(&commands, &session, "correct", &correction).unwrap();
        let revoke = format!("/forget {id}");
        run(&commands, &session, "forget", &revoke).unwrap();
        run(&commands, &session, "forget", &revoke).unwrap();
        let state = admin.0.lock().unwrap();
        assert_eq!(state.writes, 3);
        let saved = &state.snapshots[&scope(&session)];
        assert_eq!(saved.evidence.len(), 3);
        assert_eq!(saved.preferences[0].status, PreferenceStatus::Revoked);
        assert_eq!(saved.preferences[0].history.len(), 3);
        assert_eq!(saved.preferences[0].text, "新偏好\n第二行");
        assert!(
            matches!(&saved.evidence[1].source, EvidenceSource::UserStatement { text, .. } if text == &correction)
        );
        drop(state);
        let replay = run(&commands, &session, "remember", "/remember 原偏好")
            .unwrap()
            .unwrap();
        assert!(replay.contains("目前已撤销"));
        assert!(!replay.contains("偏好已保存"));
        assert_eq!(admin.0.lock().unwrap().writes, 3);
        let listing = run(&commands, &session, "listing", "/memories")
            .unwrap()
            .unwrap();
        assert!(listing.contains("已撤销"));
    }

    #[test]
    fn pagination_is_bounded_and_never_truncates_saved_evidence() {
        let (admin, commands, session) = setup();
        for number in 0..11 {
            let body = format!("第{number}条 {}", "好".repeat(300));
            run(
                &commands,
                &session,
                &format!("m-{number}"),
                &format!("/remember {body}"),
            )
            .unwrap();
        }
        let first = run(&commands, &session, "list-1", "/memories 1")
            .unwrap()
            .unwrap();
        let second = run(&commands, &session, "list-2", "/memories 2")
            .unwrap()
            .unwrap();
        let invalid = run(
            &commands,
            &session,
            "list-max",
            &format!("/memories {}", usize::MAX),
        )
        .unwrap()
        .unwrap();
        assert_eq!(first.lines().count(), 11);
        assert_eq!(second.lines().count(), 2);
        assert!(first.contains("第 1/2 页"));
        assert!(second.contains("第 2/2 页"));
        assert!(invalid.contains("共有 2 页"));
        assert!(first.contains('…'));
        assert!(first.len() < 10 * 800);
        let state = admin.0.lock().unwrap();
        assert_eq!(state.calls.len(), 11);
        for preference in &state.snapshots[&scope(&session)].preferences {
            assert!(preference.text.len() > PREVIEW_BYTES);
        }
        for evidence in &state.snapshots[&scope(&session)].evidence {
            let EvidenceSource::UserStatement { text, .. } = &evidence.source else {
                panic!()
            };
            assert!(text.starts_with("/remember 第"));
            assert!(text.len() > PREVIEW_BYTES);
        }
    }

    #[test]
    fn malformed_and_oversized_inputs_have_no_write() {
        let (admin, commands, session) = setup();
        for text in [
            "/remember".to_owned(),
            "/correct-memory id".to_owned(),
            "/memories 0".to_owned(),
            format!("/remember {}", "a".repeat(MAX_PREFERENCE_BYTES + 1)),
            "/remember text\0text".to_owned(),
            format!("/forget {}", "a".repeat(257)),
        ] {
            let reply = run(&commands, &session, "m", &text).unwrap().unwrap();
            assert!(!reply.contains("已保存"));
        }
        assert!(
            run(&commands, &session, "m", "ordinary /remember text")
                .unwrap()
                .is_none()
        );
        assert_eq!(admin.0.lock().unwrap().calls.len(), 0);
        assert!(matches!(
            run(&commands, &session, "bad\nid", "/remember 文本"),
            Err(PluginError::Task(_))
        ));
        assert_eq!(admin.0.lock().unwrap().calls.len(), 0);
    }

    #[test]
    fn known_rejections_are_explicit_without_cas_retry() {
        for (error, hint) in [
            (MemoryError::LimitReached, "容量已满"),
            (MemoryError::StaleRevision, "重新查看"),
            (MemoryError::Conflict, "冲突"),
            (MemoryError::NotFound, "没有"),
            (MemoryError::InvalidInput, "UTF-8"),
        ] {
            let (admin, commands, session) = setup();
            admin.0.lock().unwrap().write_error = Some(error);
            let reply = run(&commands, &session, "m", "/remember 偏好")
                .unwrap()
                .unwrap();
            assert!(reply.contains(hint), "{reply}");
            assert!(!reply.contains("已保存"));
            assert_eq!(admin.0.lock().unwrap().calls.len(), 1);
        }
    }

    #[test]
    fn uncertain_or_corrupt_state_fails_closed_without_claiming_no_write() {
        for error in [
            MemoryError::Storage,
            MemoryError::Unavailable,
            MemoryError::CorruptState,
            MemoryError::UnsupportedVersion,
        ] {
            for read in [false, true] {
                let (admin, commands, session) = setup();
                if read {
                    admin.0.lock().unwrap().read_error = Some(error.clone());
                } else {
                    admin.0.lock().unwrap().write_error = Some(error.clone());
                }
                let Err(PluginError::State(message)) =
                    run(&commands, &session, "m", "/remember 偏好")
                else {
                    panic!()
                };
                assert!(message.contains("无法确认"));
                assert!(!message.contains("未写"));
                assert_eq!(admin.0.lock().unwrap().calls.len(), usize::from(!read));
            }
        }
        let (admin, commands, session) = setup();
        admin.0.lock().unwrap().foreign_snapshot = true;
        assert!(matches!(
            run(&commands, &session, "m", "/memories"),
            Err(PluginError::State(_))
        ));
        assert!(matches!(
            run(&commands, &session, "m", "/remember 偏好"),
            Err(PluginError::State(_))
        ));
        assert!(admin.0.lock().unwrap().calls.is_empty());
    }
}
