//! 真实 PostgreSQL + loopback 模型 + 独立 eve-cognition 进程；仅显式测试库运行。
#[path = "../../llm-openai/tests/support/mod.rs"]
mod http_support;

use eve_state_postgres::ConnectionOptions;
use http_support::{Reply, Server, final_response};
use postgres::{Client, Config, NoTls};
use serde_json::{Value, json};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::Duration,
};

const MODEL_KEY: &str = "postgres-cognition-model-fixture";
const GOAL_TEXT: &str = "保存在 SQL 的私人目标，需要先确认整理范围与验收条件。";

fn options(path: &Path) -> ConnectionOptions {
    let bytes = std::fs::read(path).expect("测试凭据文件不可读");
    let options: ConnectionOptions = serde_json::from_slice(&bytes).expect("测试凭据结构无效");
    assert!(
        options.database.ends_with("_test"),
        "只允许专用 _test 数据库"
    );
    assert!(
        options.hostname == "localhost"
            || options
                .hostname
                .parse::<IpAddr>()
                .is_ok_and(|host| host.is_loopback())
            || (cfg!(unix) && options.hostname.starts_with('/')),
        "只允许本机测试数据库"
    );
    options
}

fn admin<T: Send + 'static>(
    path: &Path,
    action: impl FnOnce(&mut Client) -> T + Send + 'static,
) -> T {
    let path = path.to_owned();
    thread::spawn(move || {
        let options = options(&path);
        let mut config = Config::new();
        config
            .host(&options.hostname)
            .port(options.port)
            .dbname(&options.database)
            .user(&options.user)
            .password(&options.password)
            .application_name("eve-cognition-postgres-test")
            .connect_timeout(Duration::from_secs(5))
            .options("-c statement_timeout=5000 -c lock_timeout=5000");
        let mut client = config
            .connect(NoTls)
            .unwrap_or_else(|_| panic!("测试数据库连接失败"));
        let database: String = client
            .query_one("SELECT current_database()", &[])
            .unwrap()
            .get(0);
        assert!(database == options.database && database.ends_with("_test"));
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
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect()
    })
}

fn command(root: &Path, url: &str, database: Option<&Path>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_eve-cognition"));
    command
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--agent")
        .arg(root.join("AGENT.md"));
    if let Some(database) = database {
        command.arg("--database-config").arg(database);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("EVE_")) {
            command.env_remove(name);
        }
    }
    command
        .env("EVE_OPENAI_API_KEY", MODEL_KEY)
        .env("EVE_OPENAI_MODEL", "postgres-reflection-fixture")
        .env("EVE_OPENAI_PROTOCOL", "responses")
        .env("EVE_OPENAI_BASE_URL", url)
        .env("EVE_OPENAI_REASONING_EFFORT", "none")
        .env("EVE_OPENAI_TIMEOUT_SECONDS", "5")
        .env("EVE_OPENAI_MAX_OUTPUT_TOKENS", "256")
        .env("EVE_LLM_RESPONSE_MODE", "complete");
    command
}

struct Process(Option<Child>);
impl Process {
    async fn run(mut command: Command, database_password: &str) -> Output {
        let mut process = Self(Some(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        ));
        tokio::time::timeout(Duration::from_secs(15), async {
            while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SQL 认知子进程没有有界退出");
        let output = process.0.take().unwrap().wait_with_output().unwrap();
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains(MODEL_KEY));
            assert!(!text.contains(database_password));
        }
        output
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("CLI 应只输出一个 JSON 对象")
}

#[tokio::test]
#[ignore = "需要显式设置 EVE_POSTGRES_TEST_CONFIG，并与 persistence 测试串行运行"]
async fn sql_cognition_processes_persist_reflection_and_refuse_silent_backend_switches() {
    let database = PathBuf::from(
        std::env::var_os("EVE_POSTGRES_TEST_CONFIG")
            .expect("需要 EVE_POSTGRES_TEST_CONFIG 指定专用 _test 数据库"),
    );
    let credentials = options(&database);
    admin(&database, |client| {
        let active: i64 = client.query_one(
            "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE datname=current_database() AND application_name='eve-state-postgres'", &[]).unwrap().get(0);
        assert_eq!(active, 0, "测试库已有运行中的 Eve 后端");
        client
            .batch_execute("DROP SCHEMA IF EXISTS eve_state CASCADE")
            .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("AGENT.md"),
        "你是 Eve，只形成有待验证的准备建议。",
    )
    .unwrap();
    let artifact = json!({"summary":"已区分目标与当前仍缺少的输入。", "next_step":"询问用户希望整理的具体范围与验收方式。", "needs_user_input":true});
    let mut server = Server::start(vec![Reply::json(final_response(&artifact.to_string()))]).await;

    let mut add = command(root.path(), &server.url, Some(&database));
    add.env_remove("EVE_OPENAI_API_KEY")
        .args(["add", "--id", "sql-parent", "--text", GOAL_TEXT]);
    let added = success(Process::run(add, &credentials.password).await);
    assert_eq!(added["goals"]["waiting"], 1);
    assert!(server.requests.try_recv().is_err());
    assert!(!root.path().join("state/state.json").exists());
    let marker = root.path().join("state/state.backend.json");
    let binding = std::fs::read(&marker).unwrap();
    let binding_json: Value = serde_json::from_slice(&binding).unwrap();
    assert_eq!(binding_json["database"], credentials.database);
    assert!(binding_json.get("password").is_none());
    assert!(!String::from_utf8_lossy(&binding).contains(&credentials.password));

    let mut run = command(root.path(), &server.url, Some(&database));
    run.args(["run", "--seconds", "1", "--max-executions", "1"]);
    let first = success(Process::run(run, &credentials.password).await);
    assert_eq!(first["loop"]["model_requests"], 1);
    assert_eq!(first["loop"]["completed"], 1);
    assert_eq!(first["loop"]["admitted_tool_calls"], 0);
    assert_eq!(first["loop"]["started_tools"], 0);
    let captured = server.next().await;
    assert!(captured.headers.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(
        captured
            .headers
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {MODEL_KEY}"))
    );
    let request = captured.body;
    assert_eq!(request["tools"], json!([]));
    assert_eq!(request["model"], "postgres-reflection-fixture");
    assert!(request.to_string().contains(GOAL_TEXT));

    let mut show = command(root.path(), &server.url, Some(&database));
    show.env_remove("EVE_OPENAI_API_KEY")
        .args(["show", "--id", "sql-parent"]);
    let view = success(Process::run(show, &credentials.password).await);
    assert_eq!(view["goal"]["status"], "Waiting");
    assert_eq!(view["reflections"].as_array().unwrap().len(), 1);
    assert_eq!(view["reflections"][0]["goal"]["status"], "Completed");
    assert_eq!(view["reflections"][0]["artifact"], artifact);
    let committed = rows(&database);
    assert!(
        committed
            .iter()
            .any(|(namespace, key, _)| namespace == b"eve.cognition" && key == b"cognition.v1")
    );
    assert!(
        committed
            .iter()
            .any(|(namespace, key, _)| namespace == b"eve.session" && key == b"sessions.v1")
    );
    for (_, _, value) in &committed {
        assert!(!String::from_utf8_lossy(value).contains(MODEL_KEY));
        assert!(!String::from_utf8_lossy(value).contains(&credentials.password));
    }

    let mut restart = command(root.path(), &server.url, Some(&database));
    restart.args(["run", "--seconds", "1", "--max-executions", "1"]);
    let again = success(Process::run(restart, &credentials.password).await);
    assert_eq!(again["loop"]["submitted"], 0);
    assert_eq!(again["loop"]["model_requests"], 0);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(rows(&database), committed);

    let mut missing = command(root.path(), &server.url, None);
    missing.arg("status");
    assert!(
        !Process::run(missing, &credentials.password)
            .await
            .status
            .success()
    );
    assert_eq!(std::fs::read(&marker).unwrap(), binding);
    assert!(!root.path().join("state/state.json").exists());

    let changed_config = root.path().join("changed-database.json");
    let mut changed: Value = serde_json::from_slice(&std::fs::read(&database).unwrap()).unwrap();
    changed["database"] = json!("different_destination_test");
    std::fs::write(&changed_config, serde_json::to_vec(&changed).unwrap()).unwrap();
    let mut mismatch = command(root.path(), &server.url, Some(&changed_config));
    mismatch.arg("status");
    assert!(
        !Process::run(mismatch, &credentials.password)
            .await
            .status
            .success()
    );
    assert_eq!(std::fs::read(&marker).unwrap(), binding);
    assert_eq!(rows(&database), committed);

    let file_root = tempfile::tempdir().unwrap();
    std::fs::write(file_root.path().join("AGENT.md"), "Eve").unwrap();
    let mut local = command(file_root.path(), &server.url, None);
    local.env_remove("EVE_OPENAI_API_KEY").args([
        "add",
        "--id",
        "file-parent",
        "--text",
        "原文件目标",
    ]);
    success(Process::run(local, &credentials.password).await);
    let file_state = file_root.path().join("state/state.json");
    let file_bytes = std::fs::read(&file_state).unwrap();
    let mut switch = command(file_root.path(), &server.url, Some(&database));
    switch.arg("status");
    assert!(
        !Process::run(switch, &credentials.password)
            .await
            .status
            .success()
    );
    assert_eq!(std::fs::read(&file_state).unwrap(), file_bytes);
    assert!(!file_root.path().join("state/state.backend.json").exists());
    assert_eq!(rows(&database), committed);

    admin(&database, |client| {
        client
            .batch_execute("DROP SCHEMA eve_state CASCADE")
            .unwrap();
    });
}
