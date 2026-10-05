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
    admin(&database, |client| {
        client
            .batch_execute("DROP SCHEMA eve_state CASCADE")
            .unwrap()
    });
}
