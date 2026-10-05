//! 独立 QQ/终端进程使用同一公开 SQL 后端；仅本机专用测试库与 loopback 模型。
#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use eve_app::{ChatOptions, QqBotOptions};
use eve_qqbot_plugin::QQBOT_PLUGIN_ID;
use eve_state_postgres::ConnectionOptions;
use http_support::{Reply, Server, final_response};
use postgres::{Client, Config, NoTls};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::Write,
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const MODEL_KEY: &str = "postgres-qq-model-fixture";
const QQ_KEY: &str = "postgres-qq-app-fixture";

fn credentials(path: &Path) -> ConnectionOptions {
    let bytes = std::fs::read(path).expect("测试连接文件不可读");
    let value: ConnectionOptions = serde_json::from_slice(&bytes).expect("测试连接结构无效");
    assert!(value.database.ends_with("_test"), "只允许专用 _test 数据库");
    assert!(
        value.hostname == "localhost"
            || value
                .hostname
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
            || (cfg!(unix) && value.hostname.starts_with('/')),
        "只允许本机测试库"
    );
    value
}

fn admin<T: Send + 'static>(
    path: &Path,
    action: impl FnOnce(&mut Client) -> T + Send + 'static,
) -> T {
    let path = path.to_owned();
    thread::spawn(move || {
        let options = credentials(&path);
        let mut config = Config::new();
        config
            .host(&options.hostname)
            .port(options.port)
            .dbname(&options.database)
            .user(&options.user)
            .password(&options.password)
            .application_name("eve-qq-postgres-test")
            .connect_timeout(Duration::from_secs(5))
            .options("-c statement_timeout=5000 -c lock_timeout=5000");
        let mut client = config
            .connect(NoTls)
            .unwrap_or_else(|_| panic!("测试数据库连接失败"));
        let actual: String = client
            .query_one("SELECT current_database()", &[])
            .unwrap()
            .get(0);
        assert!(actual == options.database && actual.ends_with("_test"));
        action(&mut client)
    })
    .join()
    .expect("测试数据库管理线程失败")
}

type StoredRow = (Vec<u8>, Vec<u8>, Vec<u8>);
fn rows(path: &Path) -> Vec<StoredRow> {
    admin(path, |client| {
        client
            .query(
                "SELECT namespace,key,value FROM eve_state.entries ORDER BY namespace,key",
                &[],
            )
            .unwrap()
            .into_iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect()
    })
}

fn document(rows: &[StoredRow], namespace: &str, key: &str) -> Value {
    let row = rows
        .iter()
        .find(|(n, k, _)| n == namespace.as_bytes() && k == key.as_bytes())
        .expect("SQL 插件状态缺失");
    serde_json::from_slice(&row.2).unwrap()
}

#[derive(Clone, Copy)]
enum Entry {
    Qq,
    Console,
}

fn command(entry: Entry, root: &Path, url: &str, database: Option<&Path>) -> Command {
    let binary = match entry {
        Entry::Qq => env!("CARGO_BIN_EXE_eve-qqbot"),
        Entry::Console => env!("CARGO_BIN_EXE_eve"),
    };
    let mut command = Command::new(binary);
    command
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--agent")
        .arg(root.join("AGENT.md"));
    if let Some(database) = database {
        command.arg("--database-config").arg(database);
    }
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|name| name.starts_with("EVE_") || name.starts_with("QQBOT_"))
        {
            command.env_remove(name);
        }
    }
    command
        .env("EVE_OPENAI_API_KEY", MODEL_KEY)
        .env("EVE_OPENAI_MODEL", "postgres-qq-fixture")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "5")
        .env("EVE_LLM_RESPONSE_MODE", "complete");
    if matches!(entry, Entry::Qq) {
        let bridge = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/qqbot/test/fake-bridge.mjs");
        command
            .env("QQBOT_APP_SECRET", QQ_KEY)
            .env("QQBOT_APP_ID", "1904159860")
            .arg("--training")
            .arg("--bridge-script")
            .arg(bridge)
            .arg("--bridge-arg")
            .arg(root.join("scenario.json"));
    }
    command
}

struct Process(Option<Child>);
impl Process {
    async fn run(mut command: Command, input: &str, password: &str) -> Output {
        let mut process = Self(Some(
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        let mut stdin = process.0.as_mut().unwrap().stdin.take().unwrap();
        if let Err(error) = stdin.write_all(input.as_bytes()) {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        drop(stdin);
        tokio::time::timeout(Duration::from_secs(20), async {
            while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("测试自己启动的 Eve 子进程未有界退出");
        let output = process.0.take().unwrap().wait_with_output().unwrap();
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            for secret in [MODEL_KEY, QQ_KEY, password] {
                assert!(!text.contains(secret));
            }
        }
        output
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // 只保留和操作本测试 spawn 返回的 Child，不从 PID 文件或进程组构造目标。
        if let Some(child) = &mut self.0 {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENT.md"), "你是 Eve，请按已有记录回答。").unwrap();
    scenario(root.path(), vec![]);
    root
}

fn scenario(root: &Path, messages: Vec<Value>) {
    let events = root.join("events.jsonl");
    if events.exists() {
        std::fs::remove_file(&events).unwrap();
    }
    std::fs::write(
        root.join("scenario.json"),
        serde_json::to_vec(&json!({
            "messages": messages, "events_file": events
        }))
        .unwrap(),
    )
    .unwrap();
}

fn message(id: &str, text: &str, expected: &str) -> Value {
    json!({"id":id, "scope":"c2c", "target_id":"sql-user", "user_id":"sql-user",
        "text":text, "expected":expected})
}

fn successful(output: Output, entry: Entry) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if matches!(entry, Entry::Console) {
        return json!(String::from_utf8(output.stdout).unwrap());
    }
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["closed"], true);
    assert_eq!(summary["terminal_error"], false);
    summary
}

fn has_training(request: &Value) -> bool {
    request["input"].as_array().unwrap().iter().any(|m| {
        m["role"] == "system"
            && m["content"]
                .as_str()
                .is_some_and(|text| text.contains("主动提问训练模式"))
    })
}

fn memory_snapshot(saved: &[StoredRow]) -> Value {
    let memory = document(saved, "eve.memory", "memory.v1");
    assert_eq!(memory["format_version"], 1);
    let scopes = memory["scopes"].as_array().unwrap();
    assert_eq!(scopes.len(), 1);
    scopes[0]["snapshot"].clone()
}

fn assert_preference_context(request: &Value, expected: Option<&str>) {
    let preferences: Vec<Value> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| {
            let text = message["content"].as_str()?;
            if !text.contains("eve-confirmed-preferences-v1") {
                return None;
            }
            assert_eq!(message["role"], "system");
            Some(serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap())
        })
        .collect();
    match expected {
        Some(text) => {
            assert_eq!(preferences.len(), 1);
            assert_eq!(preferences[0]["preferences"].as_array().unwrap().len(), 1);
            assert_eq!(preferences[0]["preferences"][0]["text"], text);
        }
        None => assert!(preferences.is_empty()),
    }
}

fn containing_message(id: &str, text: &str, contains: &str) -> Value {
    let mut message = message(id, text, "");
    message["expected_contains"] = json!(contains);
    message
}

async fn sql_memory_and_cognition(root: &Path, database: &Path, password: &str) {
    const PREFERENCE: &str = "SQL_PRIVATE_MEMORY：先给简短结论。";
    const CORRECTED: &str = "SQL_CORRECTED_MEMORY：先给证据再给结论。";
    const GOAL: &str = "只在本地形成一份需要用户确认的反思草稿";
    let artifact = json!({
        "summary": "SQL_REFLECTION：范围尚未确认。",
        "next_step": "请用户补充执行范围。",
        "needs_user_input": true
    });
    let mut server = Server::start(vec![
        Reply::json(final_response("SQL 确认偏好后的回复。")),
        Reply::json(final_response(&artifact.to_string())),
        Reply::json(final_response("SQL 修正偏好后的回复。")),
        Reply::json(final_response("SQL 撤销偏好后的回复。")),
    ])
    .await;
    let url = server.url.clone();
    let enabled = || {
        let mut command = command(Entry::Qq, root, &url, Some(database));
        command.args(["--memory", "--cognition", "--cognition-max-executions", "1"]);
        command
    };

    scenario(
        root,
        vec![containing_message(
            "sql-remember",
            &format!("/remember {PREFERENCE}"),
            "偏好已保存：",
        )],
    );
    successful(Process::run(enabled(), "", password).await, Entry::Qq);
    let snapshot = memory_snapshot(&rows(database));
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["evidence"][0]["source"]["kind"], "UserStatement");
    assert_eq!(
        snapshot["evidence"][0]["source"]["text"],
        format!("/remember {PREFERENCE}")
    );
    let preference = &snapshot["preferences"][0];
    let id = preference["id"].as_str().unwrap().to_owned();
    assert_eq!(preference["text"], PREFERENCE);
    assert_eq!(preference["status"], "Confirmed");
    assert_eq!(
        preference["history"][0]["evidence_id"],
        snapshot["evidence"][0]["id"]
    );
    // 开启记忆没有补采本 suite 先前关闭记忆时产生的 SQL 聊天回执。
    assert!(server.requests.try_recv().is_err());

    scenario(
        root,
        vec![message(
            "sql-memory-chat",
            "重启后使用明确偏好",
            "SQL 确认偏好后的回复。",
        )],
    );
    successful(Process::run(enabled(), "", password).await, Entry::Qq);
    assert_preference_context(&server.next().await.body, Some(PREFERENCE));
    let before_reflection = rows(database);
    let remembered = document(&before_reflection, "eve.memory", "memory.v1");
    let snapshot = memory_snapshot(&before_reflection);
    assert_eq!(snapshot["revision"], 2);
    assert_eq!(snapshot["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(
        snapshot["evidence"][1]["source"]["kind"],
        "CompletedInteraction"
    );
    assert_eq!(
        snapshot["evidence"][1]["source"]["user_text"],
        "重启后使用明确偏好"
    );
    assert_eq!(
        snapshot["evidence"][1]["source"]["assistant_text"],
        "SQL 确认偏好后的回复。"
    );
    assert!(
        document(&before_reflection, QQBOT_PLUGIN_ID, "receipts.v1")["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["message"]["id"] == "sql-memory-chat" && entry["state"] == "Sent")
    );

    // 替身桥接不直接读取数据库；测试查询已提交的 Completed 后再打开文件门。
    let completed = root.join("sql-reflection-completed");
    std::fs::write(
        root.join("scenario.json"),
        serde_json::to_vec(&json!({"script": [
            {"send": containing_message("sql-memory-goal", &format!("/goal {GOAL}"), "待办已保存：")},
            {"wait_command": {"id":"sql-memory-goal", "type":"reply"}},
            {"wait_file": completed},
            {"send": containing_message("sql-memory-mind", "/mind", "SQL_REFLECTION：范围尚未确认。")},
            {"wait_command": {"id":"sql-memory-mind", "type":"reply"}}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let poll = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let saved = rows(database);
                if saved
                    .iter()
                    .any(|(owner, key, _)| owner == b"eve.cognition" && key == b"cognition.v1")
                {
                    let cognition = document(&saved, "eve.cognition", "cognition.v1");
                    let goals = cognition["state"]["goals"].as_object().unwrap();
                    if goals.values().any(|goal| {
                        goal["verification"] == "reflection:v1" && goal["status"] == "Completed"
                    }) {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SQL 内生反思未完成");
        std::fs::write(&completed, b"completed").unwrap();
    };
    let (output, ()) = tokio::join!(Process::run(enabled(), "", password), poll);
    successful(output, Entry::Qq);
    let reflection_request = server.next().await.body;
    assert_preference_context(&reflection_request, None);
    assert!(!has_training(&reflection_request));
    assert!(!reflection_request.to_string().contains(PREFERENCE));
    assert!(
        reflection_request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    let reflected = rows(database);
    assert_eq!(document(&reflected, "eve.memory", "memory.v1"), remembered);
    let cognition = document(&reflected, "eve.cognition", "cognition.v1");
    let goals = cognition["state"]["goals"].as_object().unwrap();
    assert_eq!(goals.len(), 2);
    let parent = goals
        .values()
        .find(|goal| goal["description"] == GOAL)
        .unwrap();
    assert_eq!(parent["status"], "Waiting");
    let child = goals
        .values()
        .find(|goal| goal["source"]["reference"] == parent["id"])
        .unwrap();
    assert_eq!(child["status"], "Completed");
    assert_eq!(child["feedback"]["verification_met"], true);
    assert_eq!(child["feedback"]["started_tools"], 0);

    scenario(
        root,
        vec![
            message("sql-remember", "/remember 重复消息不能改写偏好", "不得回复"),
            message("sql-memory-chat", "重复消息不能新建交互", "不得回复"),
            message("sql-memory-goal", "/goal 重复消息不能新建反思", "不得回复"),
        ],
    );
    let replay = successful(Process::run(enabled(), "", password).await, Entry::Qq);
    assert_eq!(replay["received"], 0);
    assert_eq!(replay["sent"], 0);
    assert_eq!(rows(database), reflected);
    assert!(server.requests.try_recv().is_err());

    for (command_id, change, confirmation, chat_id, input, reply, expected) in [
        (
            "sql-memory-correct",
            format!("/correct-memory {id} {CORRECTED}"),
            "偏好已修正：",
            "sql-memory-corrected-chat",
            "使用修正后的偏好",
            "SQL 修正偏好后的回复。",
            Some(CORRECTED),
        ),
        (
            "sql-memory-forget",
            format!("/forget {id}"),
            "偏好已撤销：",
            "sql-memory-revoked-chat",
            "撤销后继续聊天",
            "SQL 撤销偏好后的回复。",
            None,
        ),
    ] {
        scenario(
            root,
            vec![
                containing_message(command_id, &change, confirmation),
                message(chat_id, input, reply),
            ],
        );
        successful(Process::run(enabled(), "", password).await, Entry::Qq);
        assert_preference_context(&server.next().await.body, expected);
    }
    let committed = rows(database);
    let snapshot = memory_snapshot(&committed);
    assert_eq!(snapshot["revision"], 6);
    let evidence = snapshot["evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 6);
    assert_eq!(
        evidence
            .iter()
            .filter(|e| e["source"]["kind"] == "CompletedInteraction")
            .count(),
        3
    );
    let preference = &snapshot["preferences"][0];
    assert_eq!(preference["status"], "Revoked");
    assert_eq!(preference["text"], CORRECTED);
    let history = preference["history"].as_array().unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0]["text"], PREFERENCE);
    assert_eq!(history[1]["text"], CORRECTED);
    assert_eq!(history[2]["status"], "Revoked");
    assert_eq!(
        document(&committed, "eve.cognition", "cognition.v1"),
        cognition
    );
    assert!(!root.join("state/state.json").exists());
    for (_, _, bytes) in &committed {
        for secret in [MODEL_KEY, QQ_KEY, password] {
            assert!(!String::from_utf8_lossy(bytes).contains(secret));
        }
    }
    scenario(root, vec![]);
    successful(Process::run(enabled(), "", password).await, Entry::Qq);
    assert_eq!(rows(database), committed);
    assert!(server.requests.try_recv().is_err());
}

/// 模型响应由测试在读取已提交的 SQL Running 后显式放行，不靠延时捕捉中间状态。
struct LearningServer {
    url: String,
    requests: mpsc::UnboundedReceiver<(Value, oneshot::Sender<Value>)>,
    task: JoinHandle<()>,
}
impl LearningServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/responses", listener.local_addr().unwrap());
        let (sender, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut bytes = vec![];
                    let header_end = loop {
                        let mut chunk = [0; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                        assert!(bytes.len() < 1024 * 1024);
                        if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break index + 4;
                        }
                    };
                    let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                    assert!(headers.starts_with("POST /v1/responses HTTP/1.1\r\n"));
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|length| length.trim().parse().unwrap())
                        })
                        .unwrap();
                    assert!(length < 1024 * 1024);
                    while bytes.len() < header_end + length {
                        let mut chunk = [0; 4096];
                        let count = socket.read(&mut chunk).await.unwrap();
                        assert!(count > 0);
                        bytes.extend_from_slice(&chunk[..count]);
                    }
                    let request =
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                    let (reply, response) = oneshot::channel::<Value>();
                    assert!(sender.send((request, reply)).is_ok());
                    let body = serde_json::to_vec(&final_response(
                        &response.await.unwrap().to_string(),
                    ))
                    .unwrap();
                    let header = format!(
                        "HTTP/1.1 200 Fixture\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(header.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                })
                .await
                .expect("SQL 提炼模型夹具未有界完成");
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
}
impl Drop for LearningServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_sql(database: &Path, ready: impl Fn(&[StoredRow]) -> bool) -> Vec<StoredRow> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let saved = rows(database);
            if ready(&saved) {
                return saved;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SQL 状态未到达验收检查点")
}

fn learning_message(id: &str, text: &str, expected: &str) -> Value {
    let mut message = containing_message(id, text, expected);
    message["target_id"] = json!("sql-learning-user");
    message["user_id"] = json!("sql-learning-user");
    message
}

fn learning_memory(memory: &Value) -> &Value {
    &memory["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scope| is_learning_scope(scope))
        .expect("SQL 新作用域的交互记忆缺失")["snapshot"]
}

fn is_learning_scope(scope: &Value) -> bool {
    scope["snapshot"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .any(|evidence| {
            evidence["source"]["message_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("sql-learning-seed-"))
        })
}

fn script(root: &Path, steps: Vec<Value>) {
    std::fs::write(
        root.join("scenario.json"),
        serde_json::to_vec(&json!({"script": steps})).unwrap(),
    )
    .unwrap();
}

async fn sql_learning_requires_confirmation_and_does_not_replay(
    root: &Path,
    database: &Path,
    password: &str,
) {
    const PREFERENCE: &str = "SQL_LEARNED_PREFERENCE：先给简短结论，再给必要依据。";
    let mut chat = Server::start(
        (0..3)
            .map(|_| Reply::json(final_response("SQL 新作用域的普通回复。")))
            .collect(),
    )
    .await;
    let seeded = root.join("sql-learning-seeded");
    let mut steps = vec![
        json!({"send":learning_message("sql-learning-stop", "/train stop", "已结束当前会话的主动提问训练")}),
        json!({"wait_command":{"id":"sql-learning-stop", "type":"reply"}}),
    ];
    for index in 0..3 {
        let id = format!("sql-learning-seed-{index}");
        steps.push(json!({"send":learning_message(
            &id,
            &format!("第 {index} 次说明：每次回答先给简短结论，再给必要依据。"),
            "SQL 新作用域的普通回复。"
        )}));
        steps.push(json!({"wait_command":{"id":id, "type":"reply"}}));
    }
    steps.push(json!({"wait_file":seeded}));
    script(root, steps);
    let mut seed = command(Entry::Qq, root, &chat.url, Some(database));
    seed.arg("--memory");
    let poll = async {
        wait_sql(database, |saved| {
            let memory = document(saved, "eve.memory", "memory.v1");
            memory["scopes"].as_array().unwrap().iter().any(|scope| {
                is_learning_scope(scope)
                    && scope["snapshot"]["evidence"]
                        .as_array()
                        .is_some_and(|evidence| evidence.len() == 3)
            })
        })
        .await;
        std::fs::write(&seeded, b"seeded").unwrap();
    };
    let (output, ()) = tokio::join!(Process::run(seed, "", password), poll);
    successful(output, Entry::Qq);
    for _ in 0..3 {
        let request = chat.next().await.body;
        assert!(!has_training(&request));
        assert_preference_context(&request, None);
    }
    assert!(chat.requests.try_recv().is_err());
    let before = rows(database);
    let memory = document(&before, "eve.memory", "memory.v1");
    let snapshot = learning_memory(&memory);
    assert_eq!(snapshot["revision"], 3);
    assert_eq!(snapshot["preferences"], json!([]));
    assert!(
        snapshot["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|evidence| {
                evidence["source"]["kind"] == "CompletedInteraction"
                    && evidence["source"]["message_id"]
                        .as_str()
                        .unwrap()
                        .starts_with("sql-learning-seed-")
            })
    );
    let receipts = document(&before, QQBOT_PLUGIN_ID, "receipts.v1");
    for index in 0..3 {
        let id = format!("sql-learning-seed-{index}");
        assert!(
            receipts["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| { entry["message"]["id"] == id && entry["state"] == "Sent" })
        );
    }

    // 先前记忆测试的旧作用域也有三条经历；保留它并验证提炼不会跨作用域拼批。
    assert_eq!(memory["scopes"].as_array().unwrap().len(), 2);
    let mut server = LearningServer::start().await;
    let url = server.url.clone();
    let enabled = || {
        let mut command = command(Entry::Qq, root, &url, Some(database));
        command.args(["--memory", "--memory-learning"]);
        command
    };
    let completed = root.join("sql-learning-completed");
    script(root, vec![json!({"wait_file":completed})]);
    let inspect = async {
        let mut seen = vec![];
        for _ in 0..2 {
            let (request, reply) =
                tokio::time::timeout(Duration::from_secs(10), server.requests.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let input = request["input"].as_array().unwrap();
            assert_eq!(input.len(), 2);
            assert_eq!(input[0]["role"], "system");
            assert_eq!(input[1]["role"], "user");
            assert!(!has_training(&request));
            assert_preference_context(&request, None);
            assert!(
                request
                    .get("tools")
                    .is_none_or(|tools| { tools.as_array().is_some_and(Vec::is_empty) })
            );
            let batch: Value = serde_json::from_str(input[1]["content"].as_str().unwrap()).unwrap();
            let saved = rows(database);
            let learning = document(&saved, "eve.learning", "learning.v1");
            let record = learning["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["job"]["batch"]["id"] == batch["id"])
                .unwrap();
            assert_eq!(record["job"]["status"], "Running");
            assert_eq!(record["job"]["batch"], batch);
            assert_eq!(record["job"]["candidates"], json!([]));
            let source = &memory["scopes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|scope| scope["snapshot"]["scope"] == batch["scope"])
                .unwrap()["snapshot"];
            assert_eq!(record["memory_revision"], source["revision"]);
            let expected: Vec<_> = source["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|evidence| evidence["source"]["kind"] == "CompletedInteraction")
                .cloned()
                .collect();
            assert_eq!(batch["evidence"], json!(expected));
            assert_eq!(expected.len(), 3);
            assert!(!seen.contains(&batch["scope"]));
            seen.push(batch["scope"].clone());
            let ids: Vec<_> = expected.iter().map(|evidence| &evidence["id"]).collect();
            let text = if batch["scope"] == snapshot["scope"] {
                PREFERENCE
            } else {
                "SQL_OLD_SCOPE_CANDIDATE：旧作用域专属候选。"
            };
            assert!(
                reply
                    .send(json!({"candidates":[{
                        "text":text, "confidence":83, "evidence_ids":ids
                    }]}))
                    .is_ok()
            );
        }
        wait_sql(database, |saved| {
            let learning = document(saved, "eve.learning", "learning.v1");
            let jobs = learning["jobs"].as_array().unwrap();
            jobs.len() == 2
                && jobs
                    .iter()
                    .all(|record| record["job"]["status"] == "Completed")
        })
        .await;
        std::fs::write(&completed, b"completed").unwrap();
    };
    let (output, ()) = tokio::join!(Process::run(enabled(), "", password), inspect);
    successful(output, Entry::Qq);
    assert!(server.requests.try_recv().is_err());
    let extracted = rows(database);
    assert_eq!(document(&extracted, "eve.memory", "memory.v1"), memory);
    assert_eq!(
        document(&extracted, "eve.cognition", "cognition.v1"),
        document(&before, "eve.cognition", "cognition.v1")
    );
    let learning = document(&extracted, "eve.learning", "learning.v1");
    let candidate = &learning["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["job"]["batch"]["scope"] == snapshot["scope"])
        .unwrap()["job"]["candidates"][0];
    let candidate_id = candidate["id"].as_str().unwrap();
    let preference_id = format!("learned-{candidate_id}");
    let acceptance = format!("/accept-memory {candidate_id}");
    assert_eq!(candidate["draft"]["text"], PREFERENCE);
    assert_eq!(candidate["draft"]["confidence"], 83);
    assert_eq!(
        candidate["draft"]["evidence_ids"].as_array().unwrap().len(),
        3
    );
    scenario(
        root,
        vec![learning_message(
            "sql-learning-accept",
            &acceptance,
            "候选偏好已确认：",
        )],
    );
    successful(Process::run(enabled(), "", password).await, Entry::Qq);
    let accepted = rows(database);
    let accepted_memory = document(&accepted, "eve.memory", "memory.v1");
    let snapshot = learning_memory(&accepted_memory);
    assert_eq!(snapshot["revision"], 4);
    assert_eq!(snapshot["preferences"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["evidence"].as_array().unwrap().len(), 4);
    let preference = &snapshot["preferences"][0];
    assert_eq!(preference["id"], preference_id);
    assert_eq!(preference["text"], PREFERENCE);
    assert_eq!(preference["status"], "Confirmed");
    assert_eq!(
        snapshot["evidence"][3]["source"],
        json!({
            "kind":"UserStatement", "message_id":"sql-learning-accept", "text":acceptance
        })
    );
    assert_eq!(
        preference["history"][0]["evidence_id"],
        snapshot["evidence"][3]["id"]
    );
    assert_eq!(document(&accepted, "eve.learning", "learning.v1"), learning);

    // 空消息重启保持桥接存活两轮扫描：旧批次不重请求，所有 SQL 原始行逐字节不变。
    let ready = root.join("sql-learning-idle-ready");
    let idle = root.join("sql-learning-idle-completed");
    script(
        root,
        vec![json!({"touch":ready}), json!({"wait_file":idle})],
    );
    let inspect = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(650), server.requests.recv())
                .await
                .is_err()
        );
        std::fs::write(&idle, b"idle").unwrap();
    };
    let (output, ()) = tokio::join!(Process::run(enabled(), "", password), inspect);
    successful(output, Entry::Qq);
    assert_eq!(rows(database), accepted);

    scenario(
        root,
        vec![learning_message(
            "sql-learning-forget",
            &format!("/forget {preference_id}"),
            "偏好已撤销：",
        )],
    );
    successful(Process::run(enabled(), "", password).await, Entry::Qq);
    let revoked = rows(database);
    let revoked_memory = document(&revoked, "eve.memory", "memory.v1");
    let preference = &learning_memory(&revoked_memory)["preferences"][0];
    assert_eq!(preference["status"], "Revoked");
    assert_eq!(preference["history"].as_array().unwrap().len(), 2);
    scenario(
        root,
        vec![learning_message(
            "sql-learning-accept-after-forget",
            &acceptance,
            "目前已撤销：",
        )],
    );
    successful(Process::run(enabled(), "", password).await, Entry::Qq);
    let final_rows = rows(database);
    assert_eq!(
        document(&final_rows, "eve.memory", "memory.v1"),
        revoked_memory
    );
    assert_eq!(
        document(&final_rows, "eve.learning", "learning.v1"),
        learning
    );
    assert!(server.requests.try_recv().is_err());
    assert!(!root.join("state/state.json").exists());
    for (_, _, bytes) in &final_rows {
        for secret in [MODEL_KEY, QQ_KEY, password] {
            assert!(!String::from_utf8_lossy(bytes).contains(secret));
        }
    }
}

fn receipt(saved: &[StoredRow], id: &str) -> Value {
    document(saved, QQBOT_PLUGIN_ID, "receipts.v1")["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["message"]["id"] == id)
        .expect("SQL 回执缺失")
        .clone()
}

fn interaction_evidence(saved: &[StoredRow]) -> Vec<Value> {
    document(saved, "eve.memory", "memory.v1")["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|scope| scope["snapshot"]["evidence"].as_array().unwrap())
        .filter(|evidence| evidence["source"]["kind"] == "CompletedInteraction")
        .cloned()
        .collect()
}

fn segment_script(root: &Path, steps: Vec<Value>) {
    let events = root.join("sql-segment-events.jsonl");
    let error = root.join("sql-segment-error.txt");
    for path in [&events, &error] {
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    std::fs::write(
        root.join("scenario.json"),
        serde_json::to_vec(&json!({
            "script":steps, "events_file":events, "error_file":error
        }))
        .unwrap(),
    )
    .unwrap();
}

fn segment_events(root: &Path) -> Vec<Value> {
    let error = root.join("sql-segment-error.txt");
    assert!(
        !error.exists(),
        "{}",
        std::fs::read_to_string(error).unwrap_or_default()
    );
    std::fs::read_to_string(root.join("sql-segment-events.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|event: &Value| event["direction"] == "out")
        .collect()
}

fn assert_sql_parts(entry: &Value, text: &str, parts: &[&str], states: &[&str]) {
    assert_eq!(entry["reply"], text);
    assert_eq!(entry["segments"]["planner"], "paragraph-v2");
    let saved = entry["segments"]["parts"].as_array().unwrap();
    assert_eq!(saved.len(), parts.len());
    assert_eq!(parts.len(), states.len());
    for ((part, expected), state) in saved.iter().zip(parts).zip(states) {
        let start = usize::try_from(part["start"].as_u64().unwrap()).unwrap();
        let end = usize::try_from(part["end"].as_u64().unwrap()).unwrap();
        assert_eq!(&text[start..end], *expected);
        assert_eq!(part["state"], *state);
    }
}

async fn sql_segmented_delivery_imports_only_fully_sent_interactions(
    root: &Path,
    database: &Path,
    password: &str,
) {
    const COMPLETE: &str = "sql-segment-complete";
    const FAILED: &str = "sql-segment-failed";
    const INPUT: &str = "请按自然段给我完整回答。";
    let parts = [
        "可以，先给结论。",
        "这里保留完整依据，按原文顺序发送。",
        "最后补充下一步。",
    ];
    let text = parts.join("\n\n");
    let mut server = Server::start(vec![
        Reply::json(final_response(&text)),
        Reply::json(final_response(&text)),
    ])
    .await;
    let url = server.url.clone();
    let enabled = || {
        let mut command = command(Entry::Qq, root, &url, Some(database));
        command.args(["--memory", "--segmented"]);
        command
    };
    let before = rows(database);
    let memory_before = document(&before, "eve.memory", "memory.v1");
    let ready = root.join("sql-segment-last-awaiting-ack");
    let release = root.join("sql-segment-release-last");
    let finished = root.join("sql-segment-fully-imported");
    let mut complete = message(COMPLETE, INPUT, "");
    complete["expected_segments"] = json!(parts);
    complete["hold_segments"] = json!([2]);
    segment_script(
        root,
        vec![
            json!({"send":complete}),
            json!({"wait_command":{"id":COMPLETE,"type":"segment","count":3}}),
            json!({"touch":ready}),
            json!({"wait_file":release}),
            json!({"deliver_segment":{"id":COMPLETE,"index":2}}),
            json!({"wait_file":finished}),
        ],
    );
    let inspect = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("最后一段未到达等待回执检查点");
        let pending = rows(database);
        let entry = receipt(&pending, COMPLETE);
        assert_eq!(entry["state"], "ReplyPending");
        assert_sql_parts(&entry, &text, &parts, &["Sent", "Sent", "Sending"]);
        assert_eq!(document(&pending, "eve.memory", "memory.v1"), memory_before);
        std::fs::write(&release, b"release").unwrap();
        wait_sql(database, |saved| {
            receipt(saved, COMPLETE)["state"] == "Sent"
                && interaction_evidence(saved)
                    .iter()
                    .any(|evidence| evidence["source"]["message_id"] == COMPLETE)
        })
        .await;
        std::fs::write(&finished, b"imported").unwrap();
    };
    let (output, ()) = tokio::join!(Process::run(enabled(), "", password), inspect);
    let summary = successful(output, Entry::Qq);
    for (name, expected) in [
        ("received", 1),
        ("completed", 1),
        ("sent", 1),
        ("failed", 0),
    ] {
        assert_eq!(summary[name], expected);
    }
    assert!(!has_training(&server.next().await.body));
    let complete_rows = rows(database);
    assert_eq!(
        document(&complete_rows, QQBOT_PLUGIN_ID, "receipts.v1")["version"],
        2
    );
    let entry = receipt(&complete_rows, COMPLETE);
    assert_eq!(entry["state"], "Sent");
    assert_sql_parts(&entry, &text, &parts, &["Sent", "Sent", "Sent"]);
    let interactions = interaction_evidence(&complete_rows);
    assert_eq!(interactions.len(), interaction_evidence(&before).len() + 1);
    let imported: Vec<_> = interactions
        .iter()
        .filter(|evidence| evidence["source"]["message_id"] == COMPLETE)
        .collect();
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0]["source"]["user_text"], INPUT);
    assert_eq!(imported[0]["source"]["assistant_text"], text);
    let events = segment_events(root);
    assert_eq!(events.len(), 3);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["type"], "segment");
        assert_eq!(event["index"], index);
        assert_eq!(event["count"], 3);
        assert_eq!(event["text"], parts[index]);
    }

    let memory_complete = document(&complete_rows, "eve.memory", "memory.v1");
    let failure_saved = root.join("sql-segment-failure-saved");
    let mut failing = message(FAILED, "这次第二段无法送达。", "");
    failing["expected_segments"] = json!(parts);
    failing["fail_segment"] = json!(1);
    segment_script(
        root,
        vec![
            json!({"send":failing}),
            json!({"wait_command":{"id":FAILED,"type":"segment","count":2}}),
            json!({"wait_file":failure_saved}),
        ],
    );
    let inspect = async {
        wait_sql(database, |saved| {
            document(saved, QQBOT_PLUGIN_ID, "receipts.v1")["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["message"]["id"] == FAILED && entry["state"] == "Failed")
        })
        .await;
        std::fs::write(&failure_saved, b"failed").unwrap();
    };
    let (output, ()) = tokio::join!(Process::run(enabled(), "", password), inspect);
    let summary = successful(output, Entry::Qq);
    for (name, expected) in [
        ("received", 1),
        ("completed", 1),
        ("sent", 0),
        ("failed", 1),
    ] {
        assert_eq!(summary[name], expected);
    }
    assert!(!has_training(&server.next().await.body));
    assert!(server.requests.try_recv().is_err());
    let failed_rows = rows(database);
    let entry = receipt(&failed_rows, FAILED);
    assert_eq!(entry["state"], "Failed");
    assert_sql_parts(&entry, &text, &parts, &["Sent", "Failed", "Skipped"]);
    assert_eq!(
        document(&failed_rows, "eve.memory", "memory.v1"),
        memory_complete
    );
    let events = segment_events(root);
    assert_eq!(events.len(), 2);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["type"], "segment");
        assert_eq!(event["index"], index);
        assert_eq!(event["text"], parts[index]);
    }

    // 空启动以及平台重复投递都不补发片段，不重新调用模型，也不改任何 SQL 字节。
    for repeat in [false, true] {
        let steps = if repeat {
            [COMPLETE, FAILED]
                .into_iter()
                .flat_map(|id| {
                    let mut duplicate = message(id, "重复消息不得覆盖旧回复。", "不得回复");
                    duplicate["expected_type"] = json!("finish");
                    [
                        json!({"send":duplicate}),
                        json!({"wait_command":{"id":id,"type":"finish"}}),
                    ]
                })
                .collect()
        } else {
            vec![]
        };
        segment_script(root, steps);
        let summary = successful(Process::run(enabled(), "", password).await, Entry::Qq);
        for name in ["received", "completed", "sent", "failed"] {
            assert_eq!(summary[name], 0);
        }
        let events = segment_events(root);
        assert_eq!(events.len(), if repeat { 2 } else { 0 });
        assert!(events.iter().all(|event| event["type"] == "finish"));
        assert_eq!(rows(database), failed_rows);
        assert!(server.requests.try_recv().is_err());
    }
    for (namespace, key) in [
        ("eve.learning", "learning.v1"),
        ("eve.cognition", "cognition.v1"),
    ] {
        assert_eq!(
            document(&failed_rows, namespace, key),
            document(&before, namespace, key)
        );
    }
    assert!(!root.join("state/state.json").exists());
    for (_, _, bytes) in &failed_rows {
        for secret in [MODEL_KEY, QQ_KEY, password] {
            assert!(!String::from_utf8_lossy(bytes).contains(secret));
        }
    }
}

#[test]
fn both_entries_parse_explicit_database_paths_and_keep_file_default() {
    let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
    assert!(ChatOptions::default().database_config.is_none());
    assert!(QqBotOptions::default().database_config.is_none());
    for entry in [Entry::Qq, Entry::Console] {
        let parse = |values: &[&str]| match entry {
            Entry::Qq => QqBotOptions::parse(args(values)).map(|v| v.unwrap().database_config),
            Entry::Console => ChatOptions::parse(args(values)).map(|v| v.unwrap().database_config),
        };
        assert_eq!(
            parse(&["--database-config", "private config.json"]).unwrap(),
            Some("private config.json".into())
        );
        assert!(parse(&["--database-config"]).is_err());
        assert!(parse(&["--database-config", ""]).is_err());
    }
}

#[tokio::test]
#[ignore = "需要 EVE_POSTGRES_TEST_CONFIG；与 persistence 和 cognition_postgres 串行运行"]
async fn sql_qq_and_console_preserve_history_training_and_receipts_without_replay() {
    let database = PathBuf::from(
        std::env::var_os("EVE_POSTGRES_TEST_CONFIG")
            .expect("需要 EVE_POSTGRES_TEST_CONFIG 指定专用 _test 数据库"),
    );
    let credentials = credentials(&database);
    admin(&database, |client| {
        let active: i64 = client.query_one(
            "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE datname=current_database() AND application_name='eve-state-postgres'", &[]
        ).unwrap().get(0);
        assert_eq!(active, 0, "测试库已有运行中的 Eve 后端");
        client
            .batch_execute("DROP SCHEMA IF EXISTS eve_state CASCADE")
            .unwrap();
    });
    let root = fixture();
    let mut server = Server::start(vec![
        Reply::json(final_response("记住了 SQL 偏好。")),
        Reply::json(final_response("已经恢复普通聊天。")),
        Reply::json(final_response("重启后记得 SQL 偏好。")),
        Reply::json(final_response("终端第一轮已保存。")),
        Reply::json(final_response("终端重启后仍记得。")),
    ])
    .await;
    scenario(
        root.path(),
        vec![
            message("sql-first", "请记住 SQL 偏好", "记住了 SQL 偏好。"),
            message(
                "sql-stop",
                "/train stop",
                "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。",
            ),
            message("sql-ordinary", "继续普通聊天", "已经恢复普通聊天。"),
        ],
    );
    let first = successful(
        Process::run(
            command(Entry::Qq, root.path(), &server.url, Some(&database)),
            "",
            &credentials.password,
        )
        .await,
        Entry::Qq,
    );
    assert_eq!(first["sent"], 3);
    assert_eq!(first["failed"], 0);
    let first_request = server.next().await;
    assert!(
        first_request
            .headers
            .starts_with("POST /v1/responses HTTP/1.1")
    );
    assert!(has_training(&first_request.body));
    let ordinary = server.next().await;
    assert!(!has_training(&ordinary.body));
    let saved = rows(&database);
    assert_eq!(
        document(&saved, QQBOT_PLUGIN_ID, "receipts.v1")["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        document(&saved, QQBOT_PLUGIN_ID, "receipts.v1")["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["state"] == "Sent")
    );
    assert_eq!(
        document(&saved, "eve.training", "modes.v1")["modes"][0]["enabled"],
        false
    );
    let expressions = document(&saved, "eve.training", "expression.v1");
    let learned = expressions["rows"].as_array().unwrap();
    assert_eq!(learned.len(), 2);
    assert_eq!(learned.iter().filter(|r| r["active"] == true).count(), 1);
    assert_eq!(
        document(&saved, "eve.session", "sessions.v1")["sessions"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap()["turns"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!root.path().join("state/state.json").exists());
    let marker_path = root.path().join("state/state.backend.json");
    let marker = std::fs::read(&marker_path).unwrap();
    assert!(
        serde_json::from_slice::<Value>(&marker)
            .unwrap()
            .get("password")
            .is_none()
    );

    // 空启动不重新学习、不调用模型、不补发；重复平台 ID 也只结束本次投递。
    scenario(root.path(), vec![]);
    let idle = successful(
        Process::run(
            command(Entry::Qq, root.path(), &server.url, Some(&database)),
            "",
            &credentials.password,
        )
        .await,
        Entry::Qq,
    );
    assert_eq!(idle["sent"], 0);
    assert_eq!(idle["received"], 0);
    assert_eq!(rows(&database), saved);
    scenario(
        root.path(),
        vec![message("sql-first", "请记住 SQL 偏好", "重复消息不得回复")],
    );
    let duplicate = successful(
        Process::run(
            command(Entry::Qq, root.path(), &server.url, Some(&database)),
            "",
            &credentials.password,
        )
        .await,
        Entry::Qq,
    );
    assert_eq!(duplicate["sent"], 0);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(rows(&database), saved);
    let events = std::fs::read_to_string(root.path().join("events.jsonl")).unwrap();
    assert!(
        events
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .all(|e| e["type"] != "reply")
    );

    scenario(
        root.path(),
        vec![
            message(
                "sql-status",
                "/train status",
                "当前会话：主动提问训练已关闭。",
            ),
            message(
                "sql-recall",
                "重启后回忆之前的偏好",
                "重启后记得 SQL 偏好。",
            ),
        ],
    );
    successful(
        Process::run(
            command(Entry::Qq, root.path(), &server.url, Some(&database)),
            "",
            &credentials.password,
        )
        .await,
        Entry::Qq,
    );
    let resumed = server.next().await.body;
    assert!(!has_training(&resumed));
    assert!(
        resumed["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "assistant" && m["content"] == "记住了 SQL 偏好。")
    );

    for (input, expected) in [
        ("终端 SQL 记录\n", "终端第一轮已保存。"),
        ("终端继续\n", "终端重启后仍记得。"),
    ] {
        let output = successful(
            Process::run(
                command(Entry::Console, root.path(), &server.url, Some(&database)),
                input,
                &credentials.password,
            )
            .await,
            Entry::Console,
        );
        assert!(output.as_str().unwrap().contains(expected));
        let request = server.next().await.body;
        if input == "终端继续\n" {
            assert!(
                request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "assistant" && m["content"] == "终端第一轮已保存。")
            );
        }
    }
    let committed = rows(&database);
    for (_, _, bytes) in &committed {
        for secret in [MODEL_KEY, QQ_KEY, &credentials.password] {
            assert!(!String::from_utf8_lossy(bytes).contains(secret));
        }
    }
    successful(
        Process::run(
            command(Entry::Console, root.path(), &server.url, Some(&database)),
            "",
            &credentials.password,
        )
        .await,
        Entry::Console,
    );
    assert_eq!(rows(&database), committed);
    assert!(server.requests.try_recv().is_err());

    let changed_path = root.path().join("changed.json");
    let mut changed: Value = serde_json::from_slice(&std::fs::read(&database).unwrap()).unwrap();
    changed["database"] = json!("different_qq_destination_test");
    let mut file = std::fs::OpenOptions::new();
    file.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.mode(0o600);
    }
    file.open(&changed_path)
        .unwrap()
        .write_all(&serde_json::to_vec(&changed).unwrap())
        .unwrap();
    for entry in [Entry::Qq, Entry::Console] {
        for config in [None, Some(changed_path.as_path())] {
            let rejected = Process::run(
                command(entry, root.path(), &server.url, config),
                "",
                &credentials.password,
            )
            .await;
            assert!(!rejected.status.success());
            let error = String::from_utf8_lossy(&rejected.stderr);
            assert!(error.contains(if config.is_none() {
                "必须继续提供 --database-config"
            } else {
                "持久化绑定不一致"
            }));
            assert_eq!(std::fs::read(&marker_path).unwrap(), marker);
            assert!(!root.path().join("state/state.json").exists());
            assert_eq!(rows(&database), committed);
        }
    }

    // 使用真实默认文件宿主生成旧状态；两种入口都必须拒绝静默迁移。
    let local = fixture();
    scenario(
        local.path(),
        vec![message(
            "file-stop",
            "/train stop",
            "已结束当前会话的主动提问训练，已完成记录保留；普通聊天仍可继续。",
        )],
    );
    successful(
        Process::run(
            command(Entry::Qq, local.path(), &server.url, None),
            "",
            &credentials.password,
        )
        .await,
        Entry::Qq,
    );
    let file_path = local.path().join("state/state.json");
    let original = std::fs::read(&file_path).unwrap();
    for entry in [Entry::Qq, Entry::Console] {
        let rejected = Process::run(
            command(entry, local.path(), &server.url, Some(&database)),
            "",
            &credentials.password,
        )
        .await;
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("不能静默切换数据库"));
        assert_eq!(std::fs::read(&file_path).unwrap(), original);
        assert!(!local.path().join("state/state.backend.json").exists());
    }
    assert!(server.requests.try_recv().is_err());
    assert_eq!(rows(&database), committed);
    sql_memory_and_cognition(root.path(), &database, &credentials.password).await;
    sql_learning_requires_confirmation_and_does_not_replay(
        root.path(),
        &database,
        &credentials.password,
    )
    .await;
    sql_segmented_delivery_imports_only_fully_sent_interactions(
        root.path(),
        &database,
        &credentials.password,
    )
    .await;
    admin(&database, |client| {
        client
            .batch_execute("DROP SCHEMA eve_state CASCADE")
            .unwrap()
    });
}
