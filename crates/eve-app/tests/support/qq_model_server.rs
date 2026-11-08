//! PostgreSQL QQ 测试独用：后台交流学习不消耗前台回复或偏好提炼的放行通道。
use eve_training_api::{ConversationMessage, ConversationRole};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

const CONVERSATION: &str = "你整理 Eve 从真实交流中学到的交往经验。";
type BatchKey = (String, String, String);
type ScopeKey = (String, String);

pub struct Reply(Value);
impl Reply {
    pub fn json(value: Value) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug)]
pub struct Captured {
    pub headers: String,
    pub body: Value,
}

struct Recorded {
    request: Captured,
    conversation: Option<(BatchKey, u64)>,
}
#[derive(Default)]
struct Log {
    requests: Vec<Recorded>,
    errors: Vec<String>,
}
struct Pending {
    request: Captured,
    reply: Option<oneshot::Sender<Value>>,
}
enum Mode {
    Fixed(Mutex<VecDeque<Reply>>),
    Learning,
}

/// 已领取批次不能重放；原来尚未领取的事件仍可在恢复时首次学习。
pub struct RecoveryCheckpoint {
    count: usize,
    claimed: BTreeSet<BatchKey>,
    pending: BTreeMap<ScopeKey, (u64, u64)>,
}

pub struct Server {
    pub url: String,
    requests: mpsc::UnboundedReceiver<Pending>,
    log: Arc<Mutex<Log>>,
    database: PathBuf,
    task: JoinHandle<()>,
}

impl Server {
    pub async fn start(root: &Path, replies: Vec<Reply>) -> Self {
        Self::bind(root, Mode::Fixed(Mutex::new(replies.into()))).await
    }

    /// 测试读到 SQL Running 后才用 oneshot 放行偏好提炼结果。
    pub async fn learning(root: &Path) -> Self {
        Self::bind(root, Mode::Learning).await
    }

    async fn bind(root: &Path, mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let database = root.join("state/training-expressions.sqlite3");
        let log = Arc::new(Mutex::new(Log::default()));
        let (sender, requests) = mpsc::unbounded_channel();
        let shared = log.clone();
        let source = database.clone();
        let task = tokio::spawn(async move {
            let mode = Arc::new(mode);
            let mut workers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (socket, _) = match accepted {
                            Ok(value) => value,
                            Err(error) => {
                                shared.lock().unwrap().errors.push(format!("模型夹具连接失败: {error}"));
                                break;
                            }
                        };
                        let (mode, sender, log, source) =
                            (mode.clone(), sender.clone(), shared.clone(), source.clone());
                        workers.spawn(async move {
                            // 每个连接独立等待偏好结果，不能堵住前台或交流学习连接。
                            let limit = match mode.as_ref() {
                                Mode::Fixed(_) => 5,
                                Mode::Learning => 10,
                            };
                            let result = tokio::time::timeout(
                                Duration::from_secs(limit),
                                serve(socket, &mode, &sender, &log, &source),
                            ).await;
                            let error = match result {
                                Ok(Ok(())) => None,
                                Ok(Err(error)) => Some(error),
                                Err(_) => Some("SQL 模型夹具未有界完成".into()),
                            };
                            if let Some(error) = error {
                                log.lock().unwrap().errors.push(error);
                            }
                        });
                    }
                    completed = workers.join_next(), if !workers.is_empty() => {
                        if let Some(Err(error)) = completed {
                            shared.lock().unwrap().errors.push(format!("模型夹具工作线程失败: {error}"));
                        }
                    }
                }
            }
        });
        Self {
            url,
            requests,
            log,
            database,
            task,
        }
    }

    fn healthy(&self) {
        let log = self.log.lock().unwrap();
        assert!(log.errors.is_empty(), "SQL 模型夹具失败: {:?}", log.errors);
        assert!(!self.task.is_finished(), "SQL 模型夹具意外退出");
    }

    async fn pending(&mut self, seconds: u64) -> Pending {
        let pending =
            tokio::time::timeout(Duration::from_secs(seconds), self.requests.recv()).await;
        self.healthy();
        pending
            .expect("SQL 模型夹具未收到预期请求")
            .expect("SQL 模型夹具关闭")
    }

    pub async fn next(&mut self) -> Captured {
        let pending = self.pending(5).await;
        assert!(pending.reply.is_none(), "偏好提炼请求须显式放行");
        pending.request
    }

    pub async fn next_learning(&mut self) -> (Value, oneshot::Sender<Value>) {
        let pending = self.pending(10).await;
        (
            pending.request.body,
            pending.reply.expect("缺少偏好提炼放行通道"),
        )
    }

    pub fn assert_no_unexpected(&mut self) {
        self.healthy();
        assert!(
            self.requests.try_recv().is_err(),
            "存在额外前台或偏好提炼请求"
        );
    }

    /// 全量计数包含已经校验并回答的交流学习请求，不因分类而丢弃。
    pub fn checkpoint(&self) -> usize {
        self.healthy();
        self.log.lock().unwrap().requests.len()
    }

    pub fn assert_no_requests_since(&self, checkpoint: usize) {
        assert_eq!(
            self.checkpoint(),
            checkpoint,
            "禁止请求阶段出现了额外模型请求"
        );
    }

    pub fn recovery_checkpoint(&self) -> RecoveryCheckpoint {
        let count = self.checkpoint();
        let mut checkpoint = RecoveryCheckpoint {
            count,
            claimed: self
                .log
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter_map(|r| r.conversation.as_ref().map(|(key, _)| key.clone()))
                .collect(),
            pending: BTreeMap::new(),
        };
        if !self.database.exists() {
            return checkpoint;
        }
        let db =
            Connection::open_with_flags(&self.database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut batches = db
            .prepare("SELECT session_id,user_id,batch_id FROM conversation_batches")
            .unwrap();
        for key in batches
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
        {
            checkpoint.claimed.insert(key.unwrap());
        }
        let mut scopes = db
            .prepare("SELECT session_id,user_id,next_sequence,checkpoint FROM conversation_scopes")
            .unwrap();
        let saved = scopes
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                ))
            })
            .unwrap();
        for row in saved {
            let (session, user, next, bytes) = row.unwrap();
            let value: Value = bytes.map_or(Value::Null, |b| serde_json::from_slice(&b).unwrap());
            if let Some(id) = value["inflight"]["batch"]["id"].as_str() {
                checkpoint
                    .claimed
                    .insert((session.clone(), user.clone(), id.to_owned()));
            }
            let cursor = value["inflight"]["after"]["sequence"]
                .as_u64()
                .or(value["cursor"]["sequence"].as_u64())
                .unwrap_or(1);
            if cursor < next {
                checkpoint.pending.insert((session, user), (cursor, next));
            }
        }
        checkpoint
    }

    pub fn assert_no_replay_since(&self, checkpoint: &RecoveryCheckpoint) {
        self.healthy();
        let log = self.log.lock().unwrap();
        for recorded in &log.requests[checkpoint.count..] {
            let (key, _) = recorded
                .conversation
                .as_ref()
                .expect("恢复阶段出现额外前台或偏好请求");
            assert!(
                !checkpoint.claimed.contains(key),
                "恢复重新请求已领取批次: {key:?}"
            );
            let (first, next) = checkpoint
                .pending
                .get(&(key.0.clone(), key.1.clone()))
                .expect("恢复请求没有启动前尚未领取的事件");
            let data: Value = serde_json::from_str(
                recorded.request.body["input"][1]["content"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            let sequences: Vec<_> = data["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["metadata"]["sequence"].as_u64().unwrap())
                .collect();
            assert!(sequences.iter().all(|s| s < next));
            assert!(
                sequences.iter().any(|s| s >= first),
                "恢复只能首次学习原 pending 事件"
            );
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        if !std::thread::panicking() {
            let log = self.log.lock().unwrap();
            assert!(log.errors.is_empty(), "SQL 模型夹具失败: {:?}", log.errors);
        }
    }
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<Captured, String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.map_err(|e| e.to_string())?;
        require(count > 0, "模型请求头提前断开")?;
        bytes.extend_from_slice(&chunk[..count]);
        require(bytes.len() < 1024 * 1024, "模型请求头超限")?;
        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..header_end].to_vec()).map_err(|e| e.to_string())?;
    require(
        headers.starts_with("POST /v1/responses HTTP/1.1\r\n"),
        "模型请求路径或协议错误",
    )?;
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|s| s.trim().parse())
        })
        .ok_or("模型请求缺少 Content-Length")?
        .map_err(|_| "模型请求长度无效")?;
    require(length < 1024 * 1024, "模型请求正文超限")?;
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.map_err(|e| e.to_string())?;
        require(count > 0, "模型请求正文提前断开")?;
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + length])
        .map_err(|e| e.to_string())?;
    Ok(Captured { headers, body })
}

async fn serve(
    mut socket: TcpStream,
    mode: &Mode,
    sender: &mpsc::UnboundedSender<Pending>,
    log: &Mutex<Log>,
    database: &Path,
) -> Result<(), String> {
    let request = read_request(&mut socket).await?;
    let index = {
        let mut log = log.lock().unwrap();
        let index = log.requests.len();
        log.requests.push(Recorded {
            request: request.clone(),
            conversation: None,
        });
        index
    };
    let conversation = conversation_request(&request.body, database)?;
    let (status, response) = if let Some(identity) = conversation {
        let mut log = log.lock().unwrap();
        require(
            !log.requests
                .iter()
                .any(|r| r.conversation.as_ref() == Some(&identity)),
            "重复请求交流学习批次",
        )?;
        log.requests[index].conversation = Some(identity);
        (200, final_response("{\"lessons\":[]}"))
    } else {
        match mode {
            Mode::Fixed(replies) => {
                let reply = replies.lock().unwrap().pop_front();
                sender
                    .send(Pending {
                        request,
                        reply: None,
                    })
                    .map_err(|_| "模型请求接收通道关闭")?;
                reply.map_or((503, json!({})), |reply| (200, reply.0))
            }
            Mode::Learning => {
                let (reply, response) = oneshot::channel();
                sender
                    .send(Pending {
                        request,
                        reply: Some(reply),
                    })
                    .map_err(|_| "偏好提炼接收通道关闭")?;
                let response = response.await.map_err(|_| "偏好提炼结果未放行")?;
                (200, final_response(&response.to_string()))
            }
        }
    };
    let body = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
    let header = format!(
        "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    // 进程取消请求可以关闭连接；协议和来源错误则始终进入上面的全量错误记录。
    if socket.write_all(header.as_bytes()).await.is_ok() {
        let _ = socket.write_all(&body).await;
    }
    Ok(())
}

fn conversation_request(
    request: &Value,
    database: &Path,
) -> Result<Option<(BatchKey, u64)>, String> {
    let Some(input) = request["input"].as_array() else {
        return Ok(None);
    };
    if !input.iter().any(|m| {
        m["role"] == "system"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.starts_with(CONVERSATION))
    }) {
        return Ok(None);
    }
    require(
        input.len() == 2
            && input[0]["role"] == "system"
            && input[1]["role"] == "user"
            && input[0]["content"]
                .as_str()
                .is_some_and(|s| s.starts_with(CONVERSATION)),
        "交流学习消息布局错误",
    )?;
    require(
        request
            .get("tools")
            .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "交流学习不应带工具",
    )?;
    require(
        request["text"]["format"]["type"] == "json_object",
        "交流学习缺少 JSON 输出协议",
    )?;
    let data: Value =
        serde_json::from_str(input[1]["content"].as_str().ok_or("交流学习正文不是文本")?)
            .map_err(|_| "交流学习正文不是 JSON")?;
    let object = data.as_object().ok_or("交流学习正文不是对象")?;
    let fields = [
        "id",
        "scope",
        "messages",
        "lessons",
        "attempt",
        "feedback_pairs",
    ];
    require(
        fields.iter().all(|key| object.contains_key(*key))
            && object
                .keys()
                .all(|key| fields.contains(&key.as_str()) || key == "lesson_history"),
        "交流学习批次字段错误",
    )?;
    require(
        data["lessons"].is_array()
            && data["feedback_pairs"].is_array()
            && data.get("lesson_history").is_none_or(Value::is_array),
        "交流学习批次列表错误",
    )?;
    let session = data["scope"]["session_id"]
        .as_str()
        .ok_or("交流学习缺少会话")?;
    let user = data["scope"]["user_id"]
        .as_str()
        .ok_or("交流学习缺少用户")?;
    let id = data["id"].as_str().ok_or("交流学习缺少批次编号")?;
    let attempt = data["attempt"].as_u64().ok_or("交流学习缺少尝试次数")?;
    require(
        !session.is_empty() && !user.is_empty() && !id.is_empty() && (1..=3).contains(&attempt),
        "交流学习身份或次数无效",
    )?;
    let messages: Vec<ConversationMessage> =
        serde_json::from_value(data["messages"].clone()).map_err(|_| "交流学习消息格式无效")?;
    require(
        !messages.is_empty() && messages.len() <= eve_training_api::MAX_CONVERSATION_MESSAGES,
        "交流学习消息数量无效",
    )?;
    let db = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "交流学习原事件库不可读")?;
    let mut previous = 0;
    let mut feedback = Vec::new();
    let mut last_eve = None;
    for message in &messages {
        let (sequence, bytes): (u64, Vec<u8>) = db.query_row(
            "SELECT sequence,message_json FROM conversation_events WHERE session_id=?1 AND user_id=?2 AND message_id=?3",
            params![session, user, message.id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).map_err(|_| "交流学习消息不属于同 scope 的真实事件")?;
        require(sequence > previous, "交流学习原事件顺序错误或重复")?;
        previous = sequence;
        let mut source: ConversationMessage =
            serde_json::from_slice(&bytes).map_err(|_| "交流学习原事件损坏")?;
        require(
            source.metadata.as_ref().and_then(|m| m.sequence) == Some(sequence),
            "交流学习原事件序号缺失",
        )?;
        if let Some(fragment) = message.metadata.as_ref().and_then(|m| m.fragment.as_ref()) {
            let context = source
                .metadata
                .as_ref()
                .and_then(|m| m.context.as_deref())
                .unwrap_or("");
            require(
                fragment.text_bytes == source.text.len() && fragment.context_bytes == context.len(),
                "交流学习分片原文长度错误",
            )?;
            let text = source
                .text
                .get(fragment.text_start..fragment.text_end)
                .ok_or("交流学习正文分片不是有效 UTF-8 区间")?
                .to_owned();
            let projected_context = context
                .get(fragment.context_start..fragment.context_end)
                .ok_or("交流学习上下文分片不是有效 UTF-8 区间")?
                .to_owned();
            require(
                fragment.text_start != 0
                    || fragment.text_end != source.text.len()
                    || fragment.context_start != 0
                    || fragment.context_end != context.len(),
                "完整消息不应伪装成分片",
            )?;
            source.text = text;
            let metadata = source.metadata.as_mut().unwrap();
            metadata.context = metadata.context.as_ref().map(|_| projected_context);
            metadata.fragment = Some(fragment.clone());
        }
        require(message == &source, "交流学习投影不等于真实原文及元数据")?;
        match message.role {
            ConversationRole::Eve => last_eve = Some(&message.id),
            ConversationRole::Human => {
                if let Some(eve) = last_eve {
                    feedback
                        .push(json!({"eve_message_id":eve,"later_human_message_id":message.id}));
                }
            }
        }
    }
    require(
        data["feedback_pairs"] == json!(feedback),
        "交流学习反馈对不符合真实消息顺序",
    )?;
    let finished: u64 = db.query_row(
        "SELECT count(*) FROM conversation_batches WHERE session_id=?1 AND user_id=?2 AND batch_id=?3 AND outcome!='failed'",
        params![session, user, id], |r| r.get(0),
    ).map_err(|_| "交流学习已领取批次不可读")?;
    require(finished == 0, "交流学习重放已经结束或未知结果的批次")?;
    Ok(Some((
        (session.to_owned(), user.to_owned(), id.to_owned()),
        attempt,
    )))
}

pub fn final_response(text: &str) -> Value {
    json!({"status":"completed", "error":null, "output":[
        {"type":"message", "role":"assistant", "status":"completed", "content":[
            {"type":"output_text", "text":text, "annotations":[]}
        ]}
    ]})
}
