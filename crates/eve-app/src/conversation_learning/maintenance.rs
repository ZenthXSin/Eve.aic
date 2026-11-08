//! 本机运维投递的有界历史重访；由正常学习 worker 受理，永不投递聊天消息。
use super::AppError;
use eve_llm_api::ContextScope;
use eve_training_api::{ConversationHistoryRecord, TrainingService};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    id: String,
    source: String,
    sha256: String,
    records: u64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Status {
    version: u8,
    id: String,
    sha256: String,
    records: u64,
    phase: String,
    imported_lines: u64,
    confirmed_new_events: u64,
    #[serde(default)]
    progress: Option<eve_training_api::ConversationSweepStatus>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
struct Plan {
    scope: ContextScope,
    sequences: Vec<u64>,
}

pub(super) struct Maintenance {
    directory: PathBuf,
    loaded: bool,
    request: Option<Request>,
    status: Option<Status>,
}
impl Maintenance {
    pub(super) fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            loaded: false,
            request: None,
            status: None,
        }
    }
    pub(super) fn ready(&self) -> bool {
        self.status
            .as_ref()
            .is_some_and(|status| status.phase == "running" && status.error.is_none())
    }
    pub(super) async fn tick(
        &mut self,
        training: &dyn TrainingService,
        stopped: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), AppError> {
        if !self.loaded {
            let path = self.directory.join("request.json");
            if !path.try_exists()? {
                return Ok(());
            }
            secure_directory(&self.directory)?;
            let request: Request = serde_json::from_reader(secure_file(&path)?.take(8193))?;
            if request.version != 1
                || request.id.is_empty()
                || request.id.len() > 128
                || !request
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                || request.source != "events.jsonl"
                || request.records == 0
                || request.records > 250_000
                || request.sha256.len() != 64
                || !request.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("交流历史维护请求无效".into());
            }
            let status_path = self.directory.join("status.json");
            let previous = if status_path.try_exists()? {
                Some(serde_json::from_reader::<_, Status>(
                    secure_file(&status_path)?.take(16385),
                )?)
            } else {
                None
            };
            if let Some(previous) = previous {
                if previous.id != request.id
                    || previous.sha256 != request.sha256
                    || previous.records != request.records
                {
                    return Err("已有交流维护状态；新请求须使用独立目录与明确入口".into());
                }
                self.status = Some(previous);
            } else {
                self.status = Some(Status {
                    version: 1,
                    id: request.id.clone(),
                    sha256: request.sha256.clone(),
                    records: request.records,
                    phase: "pending".into(),
                    imported_lines: 0,
                    confirmed_new_events: 0,
                    progress: None,
                    error: None,
                });
            }
            self.request = Some(request);
            self.loaded = true;
        }
        let request = self.request.clone().ok_or("交流维护请求缺失")?;
        let mut status = self.status.clone().ok_or("交流维护状态缺失")?;
        if status.error.is_some() {
            let resume = self.directory.join("resume.json");
            if !resume.try_exists()? {
                return Ok(());
            }
            let requested: serde_json::Value =
                serde_json::from_reader(secure_file(&resume)?.take(8193))?;
            if requested["id"].as_str() != Some(request.id.as_str())
                || requested["sha256"].as_str() != Some(request.sha256.as_str())
            {
                return Err("交流维护恢复请求不匹配".into());
            }
            fs::rename(&resume, self.directory.join("resume.accepted.json"))?;
            status.error = None;
        }
        if matches!(
            status.phase.as_str(),
            "completed" | "completed_with_failures" | "failed" | "budget_exhausted"
        ) {
            return Ok(());
        }
        let outcome: Result<(), AppError> = async {
            if status.phase == "pending" || status.phase == "importing" {
                status.phase = "importing".into();
                atomic_json(&self.directory.join("status.json"), &status)?;
                let source = self.directory.join(&request.source);
                let mut digest = Context::new(&SHA256);
                let mut input = secure_file(&source)?;
                if input.metadata()?.len() > 512 * 1024 * 1024 {
                    return Err("历史消息源超限".into());
                }
                let mut buffer = [0; 65536];
                loop {
                    let n = input.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    digest.update(&buffer[..n]);
                }
                let actual: String = digest
                    .finish()
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                if actual != request.sha256 {
                    return Err("历史消息源摘要不一致".into());
                }
                let mut plans: BTreeMap<(String, String), Plan> = BTreeMap::new();
                let mut lines = 0;
                // 每条导入均走公开持久契约；中断后按原 ID 重投，不生成新事件。
                use std::io::{Seek, SeekFrom};
                input.seek(SeekFrom::Start(0))?;
                for line in BufReader::new(input).lines() {
                    let line = line?;
                    if line.len() > 262144 {
                        return Err("历史消息行超限".into());
                    }
                    let record: ConversationHistoryRecord = serde_json::from_str(&line)?;
                    if record.event.sequence == 0 {
                        return Err("历史来源顺序无效".into());
                    }
                    let imported = training.import_conversation_history(&record)?;
                    let sequence = imported.sequence.ok_or("历史原文未持久保存")?;
                    let key = (
                        record.scope.session_id.clone(),
                        record.scope.user_id.clone(),
                    );
                    let plan = plans.entry(key).or_insert_with(|| Plan {
                        scope: record.scope.clone(),
                        sequences: vec![],
                    });
                    if plan.sequences.contains(&sequence) {
                        return Err("历史来源含重复消息身份".into());
                    }
                    plan.sequences.push(sequence);
                    lines += 1;
                    if lines > request.records {
                        return Err("历史来源记录数不一致".into());
                    }
                    if lines > status.imported_lines {
                        status.confirmed_new_events += u64::from(imported.inserted);
                    }
                    status.imported_lines = status.imported_lines.max(lines);
                    if lines % 256 == 0 {
                        atomic_json(&self.directory.join("status.json"), &status)?;
                        tokio::task::yield_now().await;
                        if *stopped.borrow() {
                            return Ok(());
                        }
                    }
                }
                if lines != request.records {
                    return Err("历史来源记录数不一致".into());
                }
                atomic_json(
                    &self.directory.join("plan.json"),
                    &plans.into_values().collect::<Vec<_>>(),
                )?;
                status.phase = "scheduling".into();
                atomic_json(&self.directory.join("status.json"), &status)?;
            }
            if status.phase == "scheduling" {
                let plans: Vec<Plan> = serde_json::from_reader(
                    secure_file(&self.directory.join("plan.json"))?.take(32 * 1024 * 1024),
                )?;
                for plan in plans {
                    training.request_conversation_sweep(
                        &request.id,
                        &plan.scope,
                        &plan.sequences,
                    )?;
                }
                status.phase = "running".into();
            }
            let progress = training.conversation_sweep_status(&request.id)?;
            if !progress.completed && progress.model_requests >= 4096 {
                status.phase = "budget_exhausted".into();
            }
            if progress.completed {
                status.phase = if progress.failed_batches + progress.interrupted_batches == 0 {
                    "completed"
                } else {
                    "completed_with_failures"
                }
                .into();
            }
            status.progress = Some(progress);
            Ok(())
        }
        .await;
        if let Err(error) = outcome {
            status.error = Some(error.to_string());
        }
        atomic_json(&self.directory.join("status.json"), &status)?;
        self.status = Some(status);
        Ok(())
    }
}
fn secure_file(path: &Path) -> Result<fs::File, AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::{
            fs::{MetadataExt, OpenOptionsExt},
            prelude::PermissionsExt,
        };
        let meta = path.symlink_metadata()?;
        if !meta.is_file() || meta.uid() != 0 || meta.permissions().mode() & 0o022 != 0 {
            return Err("交流维护文件必须由 root 持有且不可由其他用户写入".into());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        if file.metadata()?.ino() != meta.ino() {
            return Err("交流维护文件读取时改变".into());
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err("交流维护入口仅支持 Unix".into())
    }
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), AppError> {
    let parent = path.parent().ok_or("交流维护路径无效")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn secure_directory(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = path.symlink_metadata()?;
        if !meta.is_dir() || meta.uid() != 0 || meta.permissions().mode() & 0o022 != 0 {
            return Err("交流维护目录必须由 root 持有且不可由其他用户写入".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_kernel::{Kernel, KernelServices, backends::MemoryStateStore};
    use eve_plugin_api::{PluginId, ServiceId};
    use eve_training_api::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn local_request_imports_into_actual_training_worker_once_and_reports_durable_completion()
    {
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            // 非 root CI 核对入口确实拒绝非 root 文件；完整宿主链由 root 发布验收执行。
            let dir = tempfile::tempdir().unwrap();
            assert!(secure_directory(dir.path()).is_err());
            fs::write(dir.path().join("request.json"), b"{}").unwrap();
            assert!(secure_file(&dir.path().join("request.json")).is_err());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let database = Arc::new(
            eve_training_sqlite::SqliteExpressions::open(&dir.path().join("training.sqlite3"))
                .unwrap(),
        );
        let services = KernelServices {
            state: Arc::new(MemoryStateStore::default()),
            ..Default::default()
        };
        let registry = services.registry.clone();
        let kernel = Kernel::with_services(services);
        kernel
            .register(Box::new(
                eve_training_plugin::TrainingPlugin::new(true)
                    .unwrap()
                    .with_conversation_store(database),
            ))
            .unwrap();
        kernel
            .start(&PluginId::new(TRAINING_PLUGIN_ID).unwrap())
            .await
            .unwrap();
        let training = registry
            .get(&ServiceId::new(TRAINING_SERVICE_ID).unwrap())
            .unwrap()
            .unwrap()
            .value
            .downcast::<TrainingServiceHandle>()
            .unwrap()
            .0
            .clone();
        let scope = ContextScope {
            session_id: "original-session".into(),
            user_id: "original-user".into(),
        };
        let mut body = Vec::new();
        for index in 0..4 {
            let record = ConversationHistoryRecord {
                scope: scope.clone(),
                event: ConversationEvent {
                    sequence: index + 1,
                    message: ConversationMessage {
                        id: format!("history-{index}"),
                        speaker: "speaker".into(),
                        role: ConversationRole::Human,
                        text: format!("真实历史原文 {index}"),
                        metadata: Some(ConversationMetadata {
                            ordering_uncertain: true,
                            ..Default::default()
                        }),
                    },
                },
            };
            serde_json::to_writer(&mut body, &record).unwrap();
            body.push(b'\n');
        }
        fs::write(dir.path().join("events.jsonl"), &body).unwrap();
        let sha256: String = ring::digest::digest(&SHA256, &body)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        atomic_json(
            &dir.path().join("request.json"),
            &Request {
                version: 1,
                id: "local-full".into(),
                source: "events.jsonl".into(),
                sha256,
                records: 4,
            },
        )
        .unwrap();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let mut maintenance = Maintenance::new(dir.path().into());
        maintenance.tick(training.as_ref(), &stopped).await.unwrap();
        let batch = training
            .take_conversation_maintenance_batch(100000, 4096)
            .unwrap()
            .unwrap();
        assert_eq!(batch.messages.len(), 4);
        training
            .finish_conversation_batch(&batch, vec![], 100001)
            .unwrap();
        maintenance.tick(training.as_ref(), &stopped).await.unwrap();
        let status: Status =
            serde_json::from_slice(&fs::read(dir.path().join("status.json")).unwrap()).unwrap();
        assert_eq!(status.phase, "completed");
        assert_eq!(status.imported_lines, 4);
        assert_eq!(status.progress.unwrap().processed, 4);
        let mut restored = Maintenance::new(dir.path().into());
        restored.tick(training.as_ref(), &stopped).await.unwrap();
        assert!(
            training
                .take_conversation_maintenance_batch(200000, 4096)
                .unwrap()
                .is_none()
        );
    }
}
