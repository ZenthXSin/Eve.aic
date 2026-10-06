//! 显式专用 PostgreSQL 测试库的只读验收；不允许隐式连接或覆盖业务库。
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
            .application_name("eve-read-only-test-admin")
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

fn schema_exists(path: &Path) -> bool {
    admin(path, |client| {
        client
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname='eve_state')",
                &[],
            )
            .unwrap()
            .get(0)
    })
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

#[tokio::test(flavor = "current_thread")]
#[ignore = "需要显式 EVE_POSTGRES_TEST_CONFIG，且与其他 SQL 验收串行运行"]
async fn read_only_postgres_preserves_values_and_refuses_schema_initialization_or_repair() {
    let path = PathBuf::from(
        std::env::var_os("EVE_POSTGRES_TEST_CONFIG").expect("缺少显式 PostgreSQL 测试配置"),
    );
    let _ = options(&path);
    admin(&path, |client| {
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

    assert!(PostgresStateStore::connect_read_only(options(&path)).is_err());
    assert!(!schema_exists(&path), "只读打开不能初始化缺失 schema");

    let namespace = PluginId::new("read-only.fixture").unwrap();
    let value = vec![0, 255, 128, 13, 10, 42];
    let writable = PostgresStateStore::connect(options(&path)).unwrap();
    writable
        .set(&namespace, "原始键\0suffix".into(), value.clone())
        .unwrap();
    writable
        .set(&namespace, "empty".into(), Vec::new())
        .unwrap();
    assert!(PostgresStateStore::connect_read_only(options(&path)).is_err());
    drop(writable);
    let saved = rows(&path);

    let read_only = PostgresStateStore::connect_read_only(options(&path)).unwrap();
    assert_eq!(
        read_only.get(&namespace, "原始键\0suffix").unwrap(),
        Some(value.clone())
    );
    assert_eq!(read_only.get(&namespace, "empty").unwrap(), Some(vec![]));
    assert_eq!(read_only.get(&namespace, "missing").unwrap(), None);
    assert!(PostgresStateStore::connect(options(&path)).is_err());
    assert!(PostgresStateStore::connect_read_only(options(&path)).is_err());
    for key in ["原始键\0suffix", "new-key"] {
        assert!(
            read_only
                .set(&namespace, key.into(), b"must-not-write".to_vec())
                .is_err()
        );
        assert_eq!(
            read_only.get(&namespace, "原始键\0suffix").unwrap(),
            Some(value.clone()),
            "拒绝写入后仍须可读"
        );
    }
    assert_eq!(rows(&path), saved);
    drop(read_only);
    let reopened = PostgresStateStore::connect_read_only(options(&path)).unwrap();
    assert_eq!(reopened.get(&namespace, "new-key").unwrap(), None);
    drop(reopened);
    assert_eq!(rows(&path), saved);

    admin(&path, |client| {
        client
            .execute("UPDATE eve_state.metadata SET format_version=999", &[])
            .unwrap();
    });
    assert!(PostgresStateStore::connect_read_only(options(&path)).is_err());
    let version: i32 = admin(&path, |client| {
        client
            .query_one("SELECT format_version FROM eve_state.metadata", &[])
            .unwrap()
            .get(0)
    });
    assert_eq!(version, 999);
    assert_eq!(rows(&path), saved);

    admin(&path, |client| {
        client
            .batch_execute(
                "UPDATE eve_state.metadata SET format_version=1; \
                 ALTER TABLE eve_state.entries ADD COLUMN unexpected TEXT",
            )
            .unwrap();
    });
    assert!(PostgresStateStore::connect_read_only(options(&path)).is_err());
    let unexpected_present: bool = admin(&path, |client| {
        client.query_one(
            "SELECT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_schema='eve_state' AND table_name='entries' AND column_name='unexpected')", &[],
        ).unwrap().get(0)
    });
    assert!(unexpected_present, "只读打开不能修复不匹配表结构");
    assert_eq!(rows(&path), saved);
}
