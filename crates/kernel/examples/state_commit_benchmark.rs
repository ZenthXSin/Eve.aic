//! 固定合成正文与十二次小更新，报告提交延迟并核对原始字节和重新打开。
use eve_kernel::backends::FileStateStore;
use eve_plugin_api::{PluginId, StateStore};
use serde_json::json;
use std::{fs, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let label = std::env::args().nth(1).ok_or("label required")?;
    let source = std::env::args().nth(2).ok_or("size required")?;
    if source == "--snapshot-copy" {
        let path = std::env::args().nth(3).ok_or("snapshot required")?;
        let original = fs::read(path)?;
        let directory = tempfile::tempdir()?;
        fs::write(directory.path().join("state.json"), &original)?;
        let before: serde_json::Value = serde_json::from_slice(&original)?;
        let started = Instant::now();
        let store = FileStateStore::open(directory.path())?;
        let open_ms = started.elapsed().as_secs_f64() * 1000.0;
        let resident = fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .map(str::to_owned);
        let owner = PluginId::new("benchmark.temporary-copy")?;
        assert!(before["entries"].get(owner.as_str()).is_none());
        let mut durations = Vec::new();
        for index in 0..12 {
            let began = Instant::now();
            store.set(&owner, "changing".into(), vec![index; 256])?;
            durations.push(began.elapsed().as_secs_f64() * 1000.0);
        }
        drop(store);
        let reopened = FileStateStore::open(directory.path())?;
        for (namespace, values) in before["entries"].as_object().ok_or("entries required")? {
            for (key, value) in values.as_object().ok_or("values required")? {
                let bytes: Vec<u8> = serde_json::from_value(value.clone())?;
                assert_eq!(reopened.get(&PluginId::new(namespace)?, key)?, Some(bytes));
            }
        }
        assert_eq!(reopened.get(&owner, "changing")?, Some(vec![11; 256]));
        let mut sorted = durations.clone();
        sorted.sort_by(f64::total_cmp);
        println!(
            "{}",
            json!({"label":label,"isolated_snapshot_copy":true,"snapshot_bytes":original.len(),
            "open_ms":open_ms,"rss_after_open":resident,"write_ms":durations,
            "median_write_ms":(sorted[5]+sorted[6])/2.0,"all_original_values_verified":true})
        );
        return Ok(());
    }
    let megabytes: usize = source.parse()?;
    assert!((1..=16).contains(&megabytes));
    let owner = PluginId::new("benchmark.synthetic")?;
    let payload: Vec<u8> = (0..=255).cycle().take(megabytes * 1024 * 1024).collect();
    let directory = tempfile::tempdir()?;
    let initial = json!({"version":1,"entries":{"benchmark.synthetic":{"unchanged":payload}}});
    fs::write(
        directory.path().join("state.json"),
        serde_json::to_vec(&initial)?,
    )?;
    drop(initial);
    let started = Instant::now();
    let store = FileStateStore::open(directory.path())?;
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    let resident = fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .map(str::to_owned);
    let mut durations = Vec::new();
    for index in 0..12 {
        let began = Instant::now();
        store.set(&owner, "changing".into(), vec![index; 256])?;
        durations.push(began.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(store.get(&owner, "unchanged")?, Some(payload.clone()));
    let snapshot_bytes = fs::metadata(directory.path().join("state.json"))?.len();
    drop(store);
    let old_format: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("state.json"))?)?;
    assert_eq!(old_format["version"], 1);
    assert_eq!(old_format.as_object().unwrap().len(), 2);
    drop(old_format);
    let reopened = FileStateStore::open(directory.path())?;
    assert_eq!(reopened.get(&owner, "unchanged")?, Some(payload));
    assert_eq!(reopened.get(&owner, "changing")?, Some(vec![11; 256]));
    let mut sorted = durations.clone();
    sorted.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({"label":label,"payload_mib":megabytes,"snapshot_bytes":snapshot_bytes,
        "open_ms":open_ms,"rss_after_open":resident,"write_ms":durations,"median_write_ms":(sorted[5]+sorted[6])/2.0,
        "original_bytes_and_restart_verified":true,"synthetic_only":true})
    );
    Ok(())
}
