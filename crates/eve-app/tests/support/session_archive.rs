use eve_session_api::{SessionArchiveStore, SessionSnapshot};
use eve_session_plugin::FileSessionArchive;
use serde_json::Value;
use std::path::Path;

/// 验收逻辑视图：从真实冷文件逐条核对长度/摘要，原始持久化仍为 v2 索引。
pub fn expand(mut document: Value, directory: &Path) -> Value {
    if document["format_version"] == 2 {
        let archive = FileSessionArchive::open(directory).unwrap();
        for history in document["sessions"].as_object_mut().unwrap().values_mut() {
            let turns: Vec<Value> = history["turns"]
                .as_array()
                .unwrap()
                .iter()
                .map(|record| {
                    let bytes = archive.get(record["hash"].as_str().unwrap()).unwrap();
                    assert_eq!(bytes.len() as u64, record["bytes"].as_u64().unwrap());
                    serde_json::from_slice(&bytes).unwrap()
                })
                .collect();
            history["turns"] = serde_json::to_value(turns).unwrap();
            serde_json::from_value::<SessionSnapshot>(history.clone())
                .unwrap()
                .validate()
                .unwrap();
        }
        document["format_version"] = 1.into();
    }
    document
}
