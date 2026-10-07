//! 真实只读记忆 CLI + 显式 PostgreSQL 专用测试库；不接触真实 QQ 或用户数据库。
use eve_kernel::{Kernel, KernelServices};
use eve_memory_api::{
    MemoryAdmin, MemoryScope, PreferenceAction, PreferenceChange, PreferenceEvidence, UserStatement,
};
use eve_memory_plugin::MemoryPlugin;
use eve_state_postgres::{ConnectionOptions, PostgresStateStore};
use postgres::{Client, Config, NoTls};
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom},
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

const CONTENT: &str = "SQL_MEMORY_PRIVATE：需要明确说明资料来源。";

fn options(path: &Path) -> ConnectionOptions {
    let options: ConnectionOptions =
        serde_json::from_slice(&std::fs::read(path).expect("无法读取显式 PostgreSQL 测试配置"))
            .expect("PostgreSQL 测试配置格式错误");
    assert!(options.database.ends_with("_test"));
    assert!(
        options.hostname == "localhost"
            || options
                .hostname
                .parse::<IpAddr>()
                .is_ok_and(|host| host.is_loopback())
            || (cfg!(unix) && options.hostname.starts_with('/'))
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
        let mut client = Config::new()
            .host(&options.hostname)
            .port(options.port)
            .dbname(&options.database)
            .user(&options.user)
            .password(&options.password)
            .application_name("eve-memory-cli-postgres-test")
            .connect_timeout(Duration::from_secs(5))
            .options("-c statement_timeout=5000 -c lock_timeout=5000")
            .connect(NoTls)
            .unwrap_or_else(|_| panic!("无法连接专用测试数据库"));
        let database: String = client
            .query_one("SELECT current_database()", &[])
            .unwrap()
            .get(0);
        assert_eq!(database, options.database);
        assert!(database.ends_with("_test"));
        action(&mut client)
    })
    .join()
    .expect("数据库测试管理线程失败")
}

type Row = (Vec<u8>, Vec<u8>, Vec<u8>);
fn rows(path: &Path) -> Vec<Row> {
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

fn scope() -> MemoryScope {
    MemoryScope {
        channel: "sql-readonly-test".into(),
        session_id: "trusted-session".into(),
        user_id: "alice".into(),
    }
}

async fn seed(database: &Path) {
    let kernel = Kernel::with_services(KernelServices {
        state: Arc::new(PostgresStateStore::connect(options(database)).unwrap()),
        ..KernelServices::default()
    });
    let plugin = MemoryPlugin::new().unwrap();
    let admin = plugin.controller();
    kernel.register(Box::new(plugin)).unwrap();
    kernel.start_all().await.unwrap();
    admin
        .update_preference(
            &scope(),
            0,
            PreferenceChange {
                operation_id: "confirm-source".into(),
                at_ms: 1,
                evidence: PreferenceEvidence::Statement(UserStatement {
                    evidence_id: "source-statement".into(),
                    message_id: "source-message".into(),
                    text: CONTENT.into(),
                    at_ms: 1,
                }),
                action: PreferenceAction::Confirm {
                    id: "source-preference".into(),
                    text: CONTENT.into(),
                },
            },
        )
        .unwrap();
    kernel.stop_all().await.unwrap();
    kernel.flush_logs().unwrap();
}

fn command(binary: &str, directory: &Path, database: Option<&Path>) -> Command {
    let mut command = Command::new(binary);
    command.arg("--state-dir").arg(directory);
    if let Some(database) = database {
        command.arg("--database-config").arg(database);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|name| name.starts_with("EVE_")) {
            command.env_remove(name);
        }
    }
    command
}

fn memory(directory: &Path, database: Option<&Path>, user: &str) -> Command {
    let mut command = command(env!("CARGO_BIN_EXE_eve-memory"), directory, database);
    let scope = scope();
    command.args([
        "--channel",
        &scope.channel,
        "--session",
        &scope.session_id,
        "--user",
        user,
    ]);
    command
}

struct Process(Option<Child>);
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn run(mut command: Command, password: &str) -> Output {
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut process = Process(Some(
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.try_clone().unwrap())
            .spawn()
            .unwrap(),
    ));
    tokio::time::timeout(Duration::from_secs(15), async {
        while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("PostgreSQL 记忆子进程未在有界时间内退出");
    let status = process.0.take().unwrap().wait().unwrap();
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains(password));
    }
    output
}

fn success(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
#[ignore = "需要显式 EVE_POSTGRES_TEST_CONFIG，且与其他 SQL 验收串行运行"]
async fn sql_memory_cli_reads_real_preferences_without_writing_or_recreating_schema() {
    let database = PathBuf::from(
        std::env::var_os("EVE_POSTGRES_TEST_CONFIG").expect("缺少显式 PostgreSQL 测试配置"),
    );
    let credentials = options(&database);
    admin(&database, |client| {
        let active: i64 = client
            .query_one(
                "SELECT count(*) FROM pg_catalog.pg_stat_activity \
                 WHERE datname=current_database() AND application_name='eve-state-postgres'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(active, 0, "测试库已有 Eve 宿主连接");
        client
            .batch_execute("DROP SCHEMA IF EXISTS eve_state CASCADE")
            .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("state");
    let mut add = command(
        env!("CARGO_BIN_EXE_eve-cognition"),
        &directory,
        Some(&database),
    );
    add.args([
        "add",
        "--id",
        "sql-read-only-parent",
        "--text",
        "只为独立测试建立真实 SQL 状态绑定。",
    ]);
    success(run(add, &credentials.password).await);
    let marker = directory.join("state.backend.json");
    let binding = std::fs::read(&marker).unwrap();
    let initial = rows(&database);
    assert!(!directory.join("state.json").exists());

    let mut empty = memory(&directory, Some(&database), "alice");
    empty.arg("status");
    let empty = success(run(empty, &credentials.password).await);
    assert_eq!(empty["read_only"], true);
    assert_eq!(empty["revision"], 0);
    assert_eq!(empty["preference_count"], 0);
    assert_eq!(rows(&database), initial, "读取空记忆不能创建记忆状态");

    seed(&database).await;
    let saved = rows(&database);
    for args in [
        vec!["status"],
        vec!["show", "--id", "source-preference"],
        vec!["evidence", "--id", "source-statement"],
    ] {
        let mut view = memory(&directory, Some(&database), "alice");
        view.args(&args);
        let view = success(run(view, &credentials.password).await);
        assert_eq!(view["read_only"], true);
        assert_eq!(view["revision"], 1);
        assert!(!view.to_string().contains(CONTENT));
        assert_eq!(rows(&database), saved);
    }
    let mut included = memory(&directory, Some(&database), "alice");
    included.args(["show", "--id", "source-preference", "--include-content"]);
    let included = success(run(included, &credentials.password).await);
    assert!(included.to_string().contains(CONTENT));
    assert_eq!(included["preference"]["current_effective"], true);

    let mut other_user = memory(&directory, Some(&database), "bob");
    other_user.arg("status");
    let other_user = success(run(other_user, &credentials.password).await);
    assert_eq!(other_user["revision"], 0);
    assert_eq!(other_user["preference_count"], 0);
    let mut missing_config = memory(&directory, None, "alice");
    missing_config.arg("status");
    assert!(
        !run(missing_config, &credentials.password)
            .await
            .status
            .success()
    );
    assert_eq!(rows(&database), saved);
    assert_eq!(std::fs::read(&marker).unwrap(), binding);
    assert!(!directory.join("state.json").exists());

    admin(&database, |client| {
        client
            .batch_execute("DROP SCHEMA eve_state CASCADE")
            .unwrap();
    });
    let mut missing_schema = memory(&directory, Some(&database), "alice");
    missing_schema.arg("status");
    assert!(
        !run(missing_schema, &credentials.password)
            .await
            .status
            .success()
    );
    let exists: bool = admin(&database, |client| {
        client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname='eve_state')",
                &[],
            )
            .unwrap()
            .get(0)
    });
    assert!(!exists, "只读 CLI 不能重新创建缺失 SQL schema");
    assert_eq!(std::fs::read(&marker).unwrap(), binding);
    assert!(!directory.join("state.json").exists());
}
