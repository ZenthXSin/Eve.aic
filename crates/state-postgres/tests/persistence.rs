//! 显式运行的本机 PostgreSQL 验收；只允许名称以 `_test` 结尾的专用数据库。
use eve_plugin_api::{PluginId, StateStore};
use eve_state_postgres::{ConnectionOptions, PostgresStateStore};
use postgres::{Client, Config, NoTls};
use std::{
    net::IpAddr,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

fn options(path: &Path) -> ConnectionOptions {
    let bytes = std::fs::read(path).expect("无法读取 EVE_POSTGRES_TEST_CONFIG 指定的测试凭据文件");
    let options: ConnectionOptions =
        serde_json::from_slice(&bytes).expect("PostgreSQL 测试凭据结构无效");
    assert!(
        options.database.ends_with("_test"),
        "破坏性验收只允许名称以 _test 结尾的专用数据库"
    );
    assert!(
        options.hostname == "localhost"
            || options
                .hostname
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
            || (cfg!(unix) && options.hostname.starts_with('/')),
        "PostgreSQL 验收只连接本机测试数据库"
    );
    options
}

// 同步 postgres 客户端只在独立普通线程内创建和销毁；业务 StateStore 则直接在
// Tokio runtime 中调用，实际检验后端自己的线程隔离，不把它包进 spawn_blocking。
fn admin<T: Send + 'static>(
    path: &Path,
    operation: impl FnOnce(&mut Client) -> T + Send + 'static,
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
            .application_name("eve-state-postgres-test-admin")
            .connect_timeout(Duration::from_secs(5))
            .options("-c statement_timeout=5000 -c lock_timeout=5000");
        let mut client = config
            .connect(NoTls)
            .unwrap_or_else(|_| panic!("无法连接专用 PostgreSQL 测试数据库"));
        let database: String = client
            .query_one("SELECT current_database()", &[])
            .expect("无法确认测试数据库")
            .get(0);
        assert!(
            database == options.database && database.ends_with("_test"),
            "测试数据库与凭据指定的安全范围不符"
        );
        operation(&mut client)
    })
    .join()
    .expect("PostgreSQL 测试管理线程失败")
}

fn backend_pids(path: &Path) -> Vec<i32> {
    admin(path, |client| {
        client
            .query(
                "SELECT pid FROM pg_catalog.pg_stat_activity \
                 WHERE datname=current_database() AND application_name='eve-state-postgres' \
                 ORDER BY pid",
                &[],
            )
            .expect("无法读取测试后端连接")
            .into_iter()
            .map(|row| row.get(0))
            .collect()
    })
}

type StoredRow = (Vec<u8>, Vec<u8>, Vec<u8>);
fn stored_rows(path: &Path) -> Vec<StoredRow> {
    admin(path, |client| {
        client
            .query(
                "SELECT namespace,key,value FROM eve_state.entries ORDER BY namespace,key",
                &[],
            )
            .expect("无法读取专用测试状态表")
            .into_iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect()
    })
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "需要显式设置 EVE_POSTGRES_TEST_CONFIG，且数据库名必须以 _test 结尾"]
async fn actual_postgres_persistence_lock_schema_and_connection_failure_are_not_hidden() {
    let path = PathBuf::from(
        std::env::var_os("EVE_POSTGRES_TEST_CONFIG")
            .expect("显式数据库验收需要 EVE_POSTGRES_TEST_CONFIG"),
    );
    let _ = options(&path);
    assert!(
        backend_pids(&path).is_empty(),
        "测试库已有 Eve 后端连接，请先停止该测试宿主"
    );
    admin(&path, |client| {
        client
            .batch_execute("DROP SCHEMA IF EXISTS eve_state CASCADE")
            .expect("无法清理专用测试 schema");
    });

    let first = PluginId::new("fixture.alpha").unwrap();
    let second = PluginId::new("fixture.beta").unwrap();
    let nul_namespace = PluginId::new("fixture\0namespace").unwrap();
    let key = "正文\0键";
    let bytes = vec![0, 255, 128, 13, 10, 42];
    let store = PostgresStateStore::connect(options(&path)).expect("首次后端初始化失败");
    assert_eq!(backend_pids(&path).len(), 1);
    assert_eq!(store.get(&first, key).unwrap(), None);
    store.set(&first, key.into(), bytes.clone()).unwrap();
    store
        .set(&second, key.into(), b"other-owner".to_vec())
        .unwrap();
    store
        .set(&first, "正文".into(), b"prefix-only".to_vec())
        .unwrap();
    store
        .set(&nul_namespace, key.into(), b"nul-owner".to_vec())
        .unwrap();
    store.set(&first, String::new(), Vec::new()).unwrap();
    assert_eq!(store.get(&first, key).unwrap(), Some(bytes.clone()));
    assert_eq!(
        store.get(&second, key).unwrap(),
        Some(b"other-owner".to_vec())
    );
    assert_eq!(
        store.get(&first, "正文").unwrap(),
        Some(b"prefix-only".to_vec())
    );
    assert_eq!(
        store.get(&nul_namespace, key).unwrap(),
        Some(b"nul-owner".to_vec())
    );
    assert_eq!(store.get(&first, "").unwrap(), Some(vec![]));
    assert_eq!(store.get(&second, "").unwrap(), None);

    // 同库第二实例必须拒绝；初始化失败不能破坏已持锁实例或把旧键返回为不存在。
    assert!(PostgresStateStore::connect(options(&path)).is_err());
    assert_eq!(backend_pids(&path).len(), 1);
    store
        .set(&first, key.into(), b"replacement".to_vec())
        .unwrap();
    assert_eq!(
        store.get(&first, key).unwrap(),
        Some(b"replacement".to_vec())
    );
    drop(store);
    assert!(backend_pids(&path).is_empty());

    let reopened = PostgresStateStore::connect(options(&path)).expect("drop 后应释放独占锁");
    assert_eq!(
        reopened.get(&first, key).unwrap(),
        Some(b"replacement".to_vec())
    );
    assert_eq!(
        reopened.get(&second, key).unwrap(),
        Some(b"other-owner".to_vec())
    );
    assert_eq!(
        reopened.get(&nul_namespace, key).unwrap(),
        Some(b"nul-owner".to_vec())
    );
    drop(reopened);
    let before_unknown_version = stored_rows(&path);

    admin(&path, |client| {
        assert_eq!(
            client
                .execute("UPDATE eve_state.metadata SET format_version=999", &[])
                .unwrap(),
            1
        );
    });
    assert!(PostgresStateStore::connect(options(&path)).is_err());
    assert!(backend_pids(&path).is_empty());
    let version: i32 = admin(&path, |client| {
        client
            .query_one("SELECT format_version FROM eve_state.metadata", &[])
            .unwrap()
            .get(0)
    });
    assert_eq!(version, 999);
    assert_eq!(stored_rows(&path), before_unknown_version);
    admin(&path, |client| {
        client
            .execute("UPDATE eve_state.metadata SET format_version=1", &[])
            .unwrap();
    });

    let disconnected = PostgresStateStore::connect(options(&path)).unwrap();
    let pids = backend_pids(&path);
    assert_eq!(pids.len(), 1);
    let pid = pids[0];
    let terminated: bool = admin(&path, move |client| {
        // 再次绑定数据库和 application_name；绝不能把其他数据库 PID 当作测试目标。
        client
            .query_one(
                "SELECT pg_catalog.pg_terminate_backend(pid) FROM pg_catalog.pg_stat_activity \
             WHERE pid=$1 AND datname=current_database() AND application_name='eve-state-postgres'",
                &[&pid],
            )
            .expect("未找到本测试实例的后端连接")
            .get(0)
    });
    assert!(terminated);
    assert!(disconnected.get(&first, key).is_err());
    assert!(
        disconnected
            .set(&first, key.into(), b"must-not-commit".to_vec())
            .is_err()
    );
    assert!(disconnected.get(&first, "absent").is_err());
    assert!(backend_pids(&path).is_empty());

    // 旧实例仍在内存中也永不重连；新实例是显式恢复，读到断连前已提交的数据。
    let healthy = PostgresStateStore::connect(options(&path)).unwrap();
    let healthy_pids = backend_pids(&path);
    assert_eq!(healthy_pids.len(), 1);
    assert_eq!(
        healthy.get(&first, key).unwrap(),
        Some(b"replacement".to_vec())
    );
    assert!(
        disconnected
            .set(&first, key.into(), b"late-retry".to_vec())
            .is_err()
    );
    assert_eq!(backend_pids(&path), healthy_pids);
    healthy
        .set(&first, key.into(), b"explicit-recovery".to_vec())
        .unwrap();
    drop(disconnected);
    assert_eq!(
        healthy.get(&first, key).unwrap(),
        Some(b"explicit-recovery".to_vec())
    );
    drop(healthy);
    assert!(backend_pids(&path).is_empty());

    let final_open = PostgresStateStore::connect(options(&path)).unwrap();
    assert_eq!(
        final_open.get(&first, key).unwrap(),
        Some(b"explicit-recovery".to_vec())
    );
    assert_eq!(final_open.get(&first, "").unwrap(), Some(vec![]));
    drop(final_open);
    admin(&path, |client| {
        client
            .batch_execute("DROP SCHEMA eve_state CASCADE")
            .expect("清理专用测试 schema 失败");
    });
}
