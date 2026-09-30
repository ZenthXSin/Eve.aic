use super::*;
use std::sync::{Arc, Barrier};

fn bootstrap(directory: &Path) -> ConfigBootstrap {
    ConfigBootstrap::new(directory, vec![runtime_llm_schema()]).with_environment(BTreeMap::new())
}
fn overrides(number: i64) -> ConfigOverrides {
    BTreeMap::from([(
        LLM_NAMESPACE.into(),
        NamespaceValues {
            schema_version: 1,
            values: BTreeMap::from([(MAX_PARALLEL_TOOL_CALLS.into(), Value::from(number))]),
        },
    )])
}
fn limit(snapshot: &ConfigSnapshot) -> usize {
    LlmRuntimeConfig::try_from(snapshot)
        .unwrap()
        .max_parallel_tool_calls
}
fn write_stored(directory: &Path, current: ConfigDocument) {
    let stored = StoredConfig {
        current,
        ..StoredConfig::default()
    };
    fs::write(
        directory.join("config.json"),
        serde_json::to_vec(&stored).unwrap(),
    )
    .unwrap();
}

#[test]
fn defaults_environment_and_file_have_defined_priority() {
    for (environment, file, expected) in [
        (None, None, 10),
        (Some("4"), None, 4),
        (Some("4"), Some(7), 7),
        // 文件覆盖时，低优先级的无效环境值不应破坏有效配置。
        (Some("invalid-value"), Some(8), 8),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut options = bootstrap(dir.path());
        if let Some(value) = environment {
            options.environment = Some(BTreeMap::from([(
                "EVE_LLM_MAX_PARALLEL_TOOL_CALLS".into(),
                value.into(),
            )]));
        }
        if let Some(value) = file {
            write_stored(
                dir.path(),
                ConfigDocument {
                    revision: 3,
                    namespaces: overrides(value),
                },
            );
        }
        let service = FileConfigService::open(&options).unwrap();
        assert_eq!(
            limit(&service.snapshot(LLM_NAMESPACE, 1).unwrap()),
            expected
        );
    }
}

#[test]
fn invalid_environment_schema_and_sensitive_fields_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = bootstrap(dir.path());
    options.environment = Some(BTreeMap::from([(
        "EVE_LLM_MAX_PARALLEL_TOOL_CALLS".into(),
        "0".into(),
    )]));
    assert!(matches!(
        FileConfigService::open(&options),
        Err(ConfigError::InvalidValue(_))
    ));
    options.environment = Some(BTreeMap::new());
    options.schemas[0]
        .fields
        .get_mut(MAX_PARALLEL_TOOL_CALLS)
        .unwrap()
        .sensitive = true;
    assert!(matches!(
        FileConfigService::open(&options),
        Err(ConfigError::UnsupportedSensitiveField(_))
    ));
    assert!(!dir.path().join("config.json").exists());
    options.schemas[0] = runtime_llm_schema();
    options.schemas.push(runtime_llm_schema());
    assert!(matches!(
        FileConfigService::open(&options),
        Err(ConfigError::InvalidSchema(_))
    ));
    options.schemas.pop();
    options.schemas[0]
        .fields
        .get_mut(MAX_PARALLEL_TOOL_CALLS)
        .unwrap()
        .default = Some(Value::from(0));
    assert!(matches!(
        FileConfigService::open(&options),
        Err(ConfigError::InvalidValue(_))
    ));
}

#[test]
fn mixed_apply_modes_preserve_request_snapshots_until_immediate_update() {
    let dir = tempfile::tempdir().unwrap();
    let service = FileConfigService::open(&bootstrap(dir.path())).unwrap();
    let first = service.begin_request(LLM_NAMESPACE, 1).unwrap();
    service
        .replace(0, overrides(11), ApplyMode::NewRequests)
        .unwrap();
    let second = service.begin_request(LLM_NAMESPACE, 1).unwrap();
    assert_eq!(limit(&service.read_request(&first).unwrap()), 10);
    assert_eq!(limit(&service.read_request(&second).unwrap()), 11);
    service
        .replace(1, overrides(12), ApplyMode::Immediate)
        .unwrap();
    service
        .replace(2, overrides(13), ApplyMode::NewRequests)
        .unwrap();
    // 即使在立即替换时尚未读取，旧请求仍只看到最近一次立即替换的值。
    assert_eq!(limit(&service.read_request(&first).unwrap()), 12);
    assert_eq!(limit(&service.read_request(&second).unwrap()), 12);
    let third = service.begin_request(LLM_NAMESPACE, 1).unwrap();
    assert_eq!(limit(&service.read_request(&third).unwrap()), 13);
    service
        .replace(3, overrides(14), ApplyMode::Immediate)
        .unwrap();
    for request in [&first, &second, &third] {
        assert_eq!(limit(&service.read_request(request).unwrap()), 14);
    }
    // DTO 可以经过序列化边界；协议不需要携带实现指针。
    let encoded = serde_json::to_vec(&third).unwrap();
    let decoded = serde_json::from_slice::<ConfigRequest>(&encoded).unwrap();
    assert_eq!(
        service.read_request(&third).unwrap(),
        service.read_request(&decoded).unwrap()
    );
}

#[test]
fn invalid_updates_and_revision_conflicts_leave_disk_and_memory_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let service = FileConfigService::open(&bootstrap(dir.path())).unwrap();
    service
        .replace(0, overrides(7), ApplyMode::Immediate)
        .unwrap();
    let bytes = fs::read(dir.path().join("config.json")).unwrap();
    let before = service.current().unwrap();
    let history = service.backups().unwrap();
    for number in [0, -1] {
        assert!(matches!(
            service.replace(1, overrides(number), ApplyMode::Immediate),
            Err(ConfigError::InvalidValue(_))
        ));
    }
    let mut wrong_version = overrides(8);
    wrong_version.get_mut(LLM_NAMESPACE).unwrap().schema_version = 2;
    assert!(matches!(
        service.replace(1, wrong_version, ApplyMode::Immediate),
        Err(ConfigError::SchemaVersion { .. })
    ));
    let mut wrong_field = overrides(8);
    wrong_field
        .get_mut(LLM_NAMESPACE)
        .unwrap()
        .values
        .insert("unknown".into(), Value::Bool(true));
    assert!(matches!(
        service.replace(1, wrong_field, ApplyMode::Immediate),
        Err(ConfigError::UnknownField(_))
    ));
    assert_eq!(
        service.replace(0, overrides(8), ApplyMode::Immediate),
        Err(ConfigError::RevisionConflict {
            expected: 0,
            found: 1
        })
    );
    assert!(matches!(
        service.snapshot(LLM_NAMESPACE, 2),
        Err(ConfigError::SchemaVersion { .. })
    ));
    assert!(matches!(
        service.snapshot("missing", 1),
        Err(ConfigError::UnknownNamespace(_))
    ));
    assert_eq!(before, service.current().unwrap());
    assert_eq!(history, service.backups().unwrap());
    assert_eq!(bytes, fs::read(dir.path().join("config.json")).unwrap());
    assert_eq!(limit(&service.snapshot(LLM_NAMESPACE, 1).unwrap()), 7);
    assert!(
        service
            .snapshot(LLM_NAMESPACE, 1)
            .unwrap()
            .get::<bool>(MAX_PARALLEL_TOOL_CALLS)
            .is_err()
    );
}

#[test]
fn restart_required_values_only_change_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = bootstrap(dir.path());
    options.schemas[0]
        .fields
        .get_mut(MAX_PARALLEL_TOOL_CALLS)
        .unwrap()
        .restart_required = true;
    let service = FileConfigService::open(&options).unwrap();
    let request = service.begin_request(LLM_NAMESPACE, 1).unwrap();
    let changed = service
        .replace(0, overrides(6), ApplyMode::Immediate)
        .unwrap();
    assert_eq!(
        changed.restart_required,
        vec![format!("{LLM_NAMESPACE}.{MAX_PARALLEL_TOOL_CALLS}")]
    );
    assert_eq!(limit(&service.snapshot(LLM_NAMESPACE, 1).unwrap()), 10);
    assert_eq!(limit(&service.read_request(&request).unwrap()), 10);
    assert_eq!(service.current().unwrap().namespaces, overrides(6));
    service.close().unwrap();
    let restored = FileConfigService::open(&options).unwrap();
    assert_eq!(limit(&restored.snapshot(LLM_NAMESPACE, 1).unwrap()), 6);
    assert_eq!(
        restored.read_request(&request),
        Err(ConfigError::StaleRequest)
    );
}

#[test]
fn required_and_optional_fields_are_validated_without_echoing_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut required = ConfigField::new(ConfigKind::String, None);
    required.environment = Some("EVE_DEMO_NAME".into());
    let mut optional = ConfigField::new(ConfigKind::Boolean, None);
    optional.required = false;
    let schema = ConfigSchema {
        namespace: "plugins.demo".into(),
        version: 1,
        fields: BTreeMap::from([("name".into(), required), ("enabled".into(), optional)]),
    };
    let options = ConfigBootstrap::new(dir.path(), vec![schema]).with_environment(BTreeMap::new());
    assert_eq!(
        FileConfigService::open(&options).err().unwrap(),
        ConfigError::MissingValue("plugins.demo.name".into())
    );
    let namespaces = BTreeMap::from([(
        "plugins.demo".into(),
        NamespaceValues {
            schema_version: 1,
            values: BTreeMap::from([("name".into(), Value::String("demo".into()))]),
        },
    )]);
    write_stored(
        dir.path(),
        ConfigDocument {
            revision: 1,
            namespaces,
        },
    );
    let service = FileConfigService::open(&options).unwrap();
    let snapshot = service.snapshot("plugins.demo", 1).unwrap();
    assert_eq!(snapshot.get::<String>("name").unwrap(), "demo");
    assert_eq!(
        snapshot.get::<bool>("enabled"),
        Err(ConfigError::MissingValue("plugins.demo.enabled".into()))
    );
    assert!(matches!(
        service.replace(1, BTreeMap::new(), ApplyMode::Immediate),
        Err(ConfigError::MissingValue(_))
    ));
    assert_eq!(service.current().unwrap().revision, 1);
}

#[test]
fn backups_are_bounded_pinnable_persistent_and_rollback_is_a_new_revision() {
    let dir = tempfile::tempdir().unwrap();
    let options = bootstrap(dir.path());
    let service = FileConfigService::open(&options).unwrap();
    service
        .replace(0, overrides(11), ApplyMode::Immediate)
        .unwrap();
    service.pin_backup(0, true).unwrap();
    for revision in 1..26 {
        service
            .replace(
                revision,
                overrides(11 + revision as i64),
                ApplyMode::NewRequests,
            )
            .unwrap();
    }
    let backups = service.backups().unwrap();
    assert_eq!(backups.len(), 21);
    assert!(backups[0].pinned && backups[0].document.revision == 0);
    assert_eq!(backups[1].document.revision, 6);
    assert_eq!(backups.last().unwrap().document.revision, 25);
    assert!(matches!(
        service.rollback(26, 1, ApplyMode::Immediate),
        Err(ConfigError::BackupNotFound(1))
    ));
    service.close().unwrap();
    let restored = FileConfigService::open(&options).unwrap();
    assert_eq!(restored.backups().unwrap(), backups);
    assert_eq!(
        restored
            .rollback(26, 0, ApplyMode::Immediate)
            .unwrap()
            .revision,
        27
    );
    assert_eq!(limit(&restored.snapshot(LLM_NAMESPACE, 1).unwrap()), 10);
    assert!(
        restored
            .backups()
            .unwrap()
            .iter()
            .any(|b| b.document.revision == 26)
    );
    restored.pin_backup(0, false).unwrap();
    assert_eq!(restored.backups().unwrap().len(), 20);
    assert!(
        !restored
            .backups()
            .unwrap()
            .iter()
            .any(|b| b.document.revision == 0)
    );
}

#[test]
fn commit_failure_preserves_current_revision_and_backups() {
    let dir = tempfile::tempdir().unwrap();
    let service = FileConfigService::open(&bootstrap(dir.path())).unwrap();
    service
        .replace(0, overrides(7), ApplyMode::Immediate)
        .unwrap();
    let saved = fs::read(dir.path().join("config.json")).unwrap();
    let history = service.backups().unwrap();
    fs::rename(
        dir.path().join("config.json"),
        dir.path().join("saved.json"),
    )
    .unwrap();
    fs::create_dir(dir.path().join("config.json")).unwrap();
    assert!(matches!(
        service.replace(1, overrides(8), ApplyMode::Immediate),
        Err(ConfigError::Storage(_))
    ));
    assert_eq!(service.current().unwrap().revision, 1);
    assert_eq!(limit(&service.snapshot(LLM_NAMESPACE, 1).unwrap()), 7);
    assert_eq!(service.backups().unwrap(), history);
    assert_eq!(fs::read(dir.path().join("saved.json")).unwrap(), saved);
    fs::remove_dir(dir.path().join("config.json")).unwrap();
    fs::rename(
        dir.path().join("saved.json"),
        dir.path().join("config.json"),
    )
    .unwrap();
    service
        .replace(1, overrides(8), ApplyMode::Immediate)
        .unwrap();
}

#[test]
fn malformed_unknown_version_and_duplicate_keys_never_clear_the_file() {
    for raw in [
        "{broken",
        r#"{"format_version":2,"current":{"revision":0,"namespaces":{}},"backups":[]}"#,
        r#"{"format_version":1,"current":{"revision":0,"namespaces":{"runtime.llm":{"schema_version":1,"values":{}},"runtime.llm":{"schema_version":1,"values":{}}}},"backups":[]}"#,
        r#"{"format_version":1,"current":{"revision":1,"namespaces":{"runtime.llm":{"schema_version":1,"values":{"max_parallel_tool_calls":2,"max_parallel_tool_calls":3}}}},"backups":[]}"#,
        r#"{"format_version":1,"current":{"revision":0,"namespaces":{}},"backups":[],"unknown":true}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), raw).unwrap();
        let options = bootstrap(dir.path());
        assert!(FileConfigService::open(&options).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json")).unwrap(),
            raw
        );
        // 错误路径必须释放目录锁，修复原文件后可立即重新打开。
        write_stored(dir.path(), ConfigDocument::default());
        assert!(FileConfigService::open(&options).is_ok());
    }
}

#[test]
fn directory_is_exclusive_and_stop_invalidates_handles_without_holding_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let options = bootstrap(dir.path());
    let service = Arc::new(FileConfigService::open(&options).unwrap());
    assert!(matches!(
        FileConfigService::open(&options),
        Err(ConfigError::Storage(_))
    ));
    let retained = service.clone();
    service.close().unwrap();
    assert_eq!(
        retained.snapshot(LLM_NAMESPACE, 1),
        Err(ConfigError::Unavailable)
    );
    assert!(matches!(
        retained.replace(0, overrides(2), ApplyMode::Immediate),
        Err(ConfigError::Unavailable)
    ));
    let fresh = FileConfigService::open(&options).unwrap();
    assert_eq!(limit(&fresh.snapshot(LLM_NAMESPACE, 1).unwrap()), 10);
}

#[cfg(unix)]
#[test]
fn final_drop_unlocks_even_while_a_duplicate_descriptor_is_alive() {
    let dir = tempfile::tempdir().unwrap();
    let options = bootstrap(dir.path());
    let service = FileConfigService::open(&options).unwrap();
    let duplicate = service
        .state()
        .unwrap()
        .lock_file
        .as_ref()
        .unwrap()
        .0
        .try_clone()
        .unwrap();
    drop(service);
    let reopened = FileConfigService::open(&options).unwrap();
    assert_eq!(limit(&reopened.snapshot(LLM_NAMESPACE, 1).unwrap()), 10);
    drop(reopened);
    drop(duplicate);
}

#[test]
fn concurrent_cas_writers_have_one_winner_and_snapshots_are_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let mut options = bootstrap(dir.path());
    options.schemas[0].fields.insert(
        "mirror".into(),
        ConfigField::new(
            ConfigKind::Integer {
                minimum: Some(1),
                maximum: None,
            },
            Some(Value::from(10)),
        ),
    );
    let service = Arc::new(FileConfigService::open(&options).unwrap());
    let barrier = Arc::new(Barrier::new(8));
    let mut threads = Vec::new();
    for value in 11..19 {
        let service = service.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let mut values = overrides(value);
            values
                .get_mut(LLM_NAMESPACE)
                .unwrap()
                .values
                .insert("mirror".into(), Value::from(value));
            barrier.wait();
            let result = service.replace(0, values, ApplyMode::Immediate);
            let snapshot = service.snapshot(LLM_NAMESPACE, 1).unwrap();
            assert_eq!(
                snapshot.get::<i64>(MAX_PARALLEL_TOOL_CALLS).unwrap(),
                snapshot.get::<i64>("mirror").unwrap()
            );
            result
        }));
    }
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(r, Err(ConfigError::RevisionConflict { .. })))
            .count(),
        7
    );
    assert_eq!(service.current().unwrap().revision, 1);
    assert_eq!(service.backups().unwrap().len(), 1);
}

#[cfg(unix)]
#[test]
fn dangling_and_regular_symlinks_are_not_treated_as_new_config() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.json");
    std::os::unix::fs::symlink(&target, dir.path().join("config.json")).unwrap();
    assert!(matches!(
        FileConfigService::open(&bootstrap(dir.path())),
        Err(ConfigError::InvalidDocument(_))
    ));
    fs::write(&target, "{}").unwrap();
    assert!(matches!(
        FileConfigService::open(&bootstrap(dir.path())),
        Err(ConfigError::InvalidDocument(_))
    ));
    assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
}
