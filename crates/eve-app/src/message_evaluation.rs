//! 在独立配置目录上比较消息判断语义，不装配消息路由、任务控制或 QQ。
use crate::{AppError, AppFailure, MessageJudgeMode, config, qq_message_judge};
use eve_config_api::{
    CONFIG_PLUGIN_ID, CONFIG_SERVICE_ID, ConfigService, ConfigServiceHandle, MODELS_NAMESPACE,
    MODELS_SCHEMA_VERSION, ModelRolesConfig, model_roles_schema,
};
use eve_config_plugin::{ConfigBootstrap, ConfigPlugin};
use eve_kernel::{Kernel, KernelServices, backends::MemoryLogger};
use eve_message_api::{
    IntentPart, MESSAGE_NAMESPACE, MessageConfig, MessageIntent, RELATION_PLUGIN_ID,
    RELATION_SERVICE_ID, RelationDecision, RelationError, RelationInput, RelationServiceHandle,
    TextSpan, message_schema,
};
use eve_plugin_api::{PluginId, ServiceId};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_DATASET_BYTES: u64 = 1_048_576;
const MAX_REPORT_BYTES: usize = 4_194_304;
const MAX_CASES: usize = 128;

pub const MESSAGE_EVALUATION_HELP: &str = "Eve 消息语义评估
用法：eve-message-evaluate [--mode rules|primary|jev] [--dataset 文件] [--output 新文件] [--state-dir 新目录]
默认 rules 完全离线；默认样本 benchmarks/messages/cases.json；默认报告写 stdout。
primary 使用明确规则→主模型；jev 使用明确规则→Jev→主模型，均复用 QQ 的判断装配。
模型模式会发送样本中的任务和消息文本；凭据只读取 EVE_OPENAI_API_KEY / EVE_JEV_API_KEY。
默认创建并清理独立临时配置目录；--state-dir 只能是尚不存在的新目录，运行后保留。
不启动 QQ、任务控制或工具；不计算任务完成率，不估计不可观测的请求数或回退率。
样本上限 1 MiB、128 例；报告上限 4 MiB；--output 拒绝覆盖已有文件。";

#[derive(Clone, Debug)]
pub struct MessageEvaluationOptions {
    pub mode: MessageJudgeMode,
    pub dataset: PathBuf,
    pub output: Option<PathBuf>,
    pub state_directory: Option<PathBuf>,
}

impl Default for MessageEvaluationOptions {
    fn default() -> Self {
        Self {
            mode: MessageJudgeMode::Off,
            dataset: "benchmarks/messages/cases.json".into(),
            output: None,
            state_directory: None,
        }
    }
}

impl MessageEvaluationOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut options = Self::default();
        let mut seen = BTreeSet::new();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            let name = arg.to_str().ok_or("评估参数必须为 UTF-8。")?;
            if !matches!(name, "--mode" | "--dataset" | "--output" | "--state-dir")
                || !seen.insert(name.to_owned())
            {
                return Err("未知或重复评估参数；使用 --help 查看用法。".into());
            }
            let value = args.next().ok_or("评估参数缺少值。")?;
            if value.is_empty() {
                return Err("评估参数值不能为空。".into());
            }
            match name {
                "--mode" => {
                    options.mode = match value.to_str() {
                        Some("rules") => MessageJudgeMode::Off,
                        Some("primary") => MessageJudgeMode::Primary,
                        Some("jev") => MessageJudgeMode::Jev,
                        _ => return Err("评估模式必须为 rules、primary 或 jev。".into()),
                    }
                }
                "--dataset" => options.dataset = value.into(),
                "--output" => options.output = Some(value.into()),
                "--state-dir" => options.state_directory = Some(value.into()),
                _ => unreachable!(),
            }
        }
        Ok(Some(options))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Dataset {
    schema_version: u32,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    note: String,
    input: RelationInput,
    accepted: Vec<Vec<ExpectedPart>>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExpectedPart {
    intent: MessageIntent,
    span: Option<TextSpan>,
}

#[derive(Serialize)]
pub struct MessageEvaluationReport {
    schema_version: u32,
    started_at_unix_ms: u64,
    mode: &'static str,
    dataset_sha256: String,
    configuration_sha256: String,
    judge_timeout_ms: u64,
    confidence_threshold: u8,
    summary: EvaluationSummary,
    cases: Vec<CaseReport>,
}

// 配置指纹用于辨别同样本、同参数比较，不把端点或凭据引用写进报告。
// 原始密钥不属于 ConfigService 的普通配置，因此也不参与指纹。
fn configuration_sha256(
    settings: &dyn ConfigService,
    mode: MessageJudgeMode,
) -> Result<String, AppError> {
    let mut snapshots = vec![settings.snapshot(MESSAGE_NAMESPACE, 1)?];
    if mode != MessageJudgeMode::Off {
        let provider = settings.snapshot(config::OPENAI_NAMESPACE, 1)?;
        let role = config::configured_role(&provider)?;
        snapshots.push(provider);
        let roles = ModelRolesConfig::capture(settings)?;
        // 只计入被当前链消费的角色，避免未使用的语义模型配置干扰比较。
        let selected = serde_json::json!({
            "primary": role.map(|role| profile_fingerprint(roles.profile(role))),
            "jev": (mode == MessageJudgeMode::Jev).then(||
                profile_fingerprint(roles.profile(eve_config_api::ModelRole::Jev))),
        });
        snapshots.push(eve_config_api::ConfigSnapshot {
            namespace: MODELS_NAMESPACE.into(),
            schema_version: MODELS_SCHEMA_VERSION,
            revision: roles.revision(),
            values: [("selected".into(), selected)].into(),
        });
        if mode == MessageJudgeMode::Jev {
            snapshots.push(settings.snapshot("provider.jev", 1)?);
        }
    }
    let bytes = serde_json::to_vec(&snapshots).map_err(|_| "无法计算评估配置指纹。")?;
    Ok(sha256(&bytes))
}

fn profile_fingerprint(profile: Option<&eve_config_api::ModelProfile>) -> serde_json::Value {
    profile.map_or(serde_json::Value::Null, |profile| serde_json::json!({
        "provider": profile.provider,
        "model": profile.model,
        "timeout_ms": profile.timeout_ms,
        "max_concurrent_requests": profile.max_concurrent_requests,
        "max_output_tokens": profile.max_output_tokens,
    }))
}

fn sha256(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Default, Serialize)]
struct EvaluationSummary {
    cases: usize,
    successful_judgements: usize,
    exact_matches: usize,
    label_matches: usize,
    raw_span_matches: usize,
    false_cancellations: usize,
    correction_required_cases: usize,
    missed_corrections: usize,
    low_confidence_or_ambiguous: usize,
    errors: usize,
    timeouts: usize,
    mean_latency_ms: f64,
}

#[derive(Serialize)]
struct CaseReport {
    id: String,
    accepted: Vec<Vec<ExpectedPart>>,
    actual: Vec<ObservedPart>,
    exact_match: bool,
    label_match: bool,
    raw_span_match: bool,
    false_cancellation: bool,
    correction_required: bool,
    missed_correction: bool,
    low_confidence_or_ambiguous: bool,
    latency_ms: f64,
    error: Option<&'static str>,
}

#[derive(Serialize)]
struct ObservedPart {
    intent: MessageIntent,
    confidence: u8,
    span: Option<TextSpan>,
    raw_text: Option<String>,
}

fn dataset(path: &PathBuf) -> Result<(Dataset, String), AppError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|_| "无法打开评估样本。")?
        .take(MAX_DATASET_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "无法读取评估样本。")?;
    if bytes.len() as u64 > MAX_DATASET_BYTES {
        return Err("评估样本超过 1 MiB。".into());
    }
    let dataset: Dataset = serde_json::from_slice(&bytes)
        .map_err(|_| "评估样本 JSON 无效、字段重复或不符合 schema。")?;
    if dataset.schema_version != 1 || dataset.cases.is_empty() || dataset.cases.len() > MAX_CASES {
        return Err("评估样本版本或数量无效。".into());
    }
    let mut ids = BTreeSet::new();
    for case in &dataset.cases {
        if case.id.is_empty()
            || case.id.len() > 80
            || !case
                .id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
            || !ids.insert(&case.id)
            || case.note.trim().is_empty()
            || case.note.len() > 2048
            || case.input.task_text.trim().is_empty()
            || case.input.task_text.len() > 32768
            || case.input.message.text.len() > 32768
            || case.input.message.validate().is_err()
            || case.accepted.is_empty()
            || case.accepted.len() > 4
        {
            return Err("评估样本标识、输入或标注无效。".into());
        }
        if let Some(question) = &case.input.clarification
            && (question.question_id.is_empty()
                || question.question_id.len() > 256
                || question.source_text.trim().is_empty()
                || question.source_text.len() > 8192
                || question.prompt.trim().is_empty()
                || question.prompt.len() > 8192)
        {
            return Err("评估样本澄清上下文无效。".into());
        }
        for accepted in &case.accepted {
            if accepted
                .iter()
                .any(|part| part.intent == MessageIntent::Answer)
                && !case.input.clarification.as_ref().is_some_and(|question| {
                    case.input.message.reply_to.as_ref() == Some(&question.question_id)
                })
            {
                return Err("评估 answer 标注必须具有当前澄清和匹配的回复引用。".into());
            }
            let decision = RelationDecision {
                target: case.input.message.target.clone(),
                message_id: case.input.message.message_id.clone(),
                parts: accepted
                    .iter()
                    .map(|part| IntentPart {
                        intent: part.intent,
                        confidence: 100,
                        span: part.span,
                    })
                    .collect(),
                explanation: "公开人工标注".into(),
            };
            decision
                .validate(&case.input)
                .map_err(|_| "评估标注的标签或原文字节范围无效。")?;
        }
    }
    Ok((dataset, sha256(&bytes)))
}

struct EvaluationDirectory {
    path: PathBuf,
    temporary: bool,
}

impl EvaluationDirectory {
    fn create(explicit: Option<PathBuf>) -> Result<Self, AppError> {
        let temporary = explicit.is_none();
        let path = match explicit {
            Some(path) => path,
            None => {
                let mut random = [0u8; 16];
                SystemRandom::new()
                    .fill(&mut random)
                    .map_err(|_| "无法生成独立评估目录。")?;
                let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
                std::env::temp_dir().join(format!("eve-message-evaluation-{suffix}"))
            }
        };
        fs::create_dir(&path)
            .map_err(|_| "评估配置目录必须尚不存在且父目录可写；禁止使用已有 QQ 状态目录。")?;
        Ok(Self { path, temporary })
    }

    fn cleanup(self) -> Result<(), AppError> {
        if self.temporary {
            fs::remove_dir_all(&self.path).map_err(|_| "无法清理本次独立临时评估目录。")?;
        }
        Ok(())
    }
}

/// 逐例串行调用已装配的 RelationService；不发送任何控制动作或执行工具。
pub async fn run_message_evaluation(
    options: MessageEvaluationOptions,
) -> Result<MessageEvaluationReport, AppError> {
    let (dataset, dataset_sha256) = dataset(&options.dataset)?;
    let started_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .ok_or("系统时间无法用于评估记录。")?;
    // 提前检测已有报告，避免完成收费请求之后才发现不能写出。
    if options.output.as_ref().is_some_and(|path| path.exists()) {
        return Err("评估报告文件已存在，拒绝覆盖。".into());
    }
    let directory = EvaluationDirectory::create(options.state_directory)?;
    let backends = KernelServices {
        logger: Arc::new(MemoryLogger::default()),
        ..KernelServices::default()
    };
    let registry = backends.registry.clone();
    let kernel = Kernel::with_services(backends);
    let result = async {
        let mut schemas = vec![message_schema()];
        if options.mode != MessageJudgeMode::Off {
            schemas.extend([
                config::openai_schema(),
                qq_message_judge::provider_schema(),
                model_roles_schema(),
            ]);
        }
        let bootstrap = ConfigBootstrap::new(directory.path.join("config"), schemas);
        kernel.register(Box::new(ConfigPlugin::new(bootstrap)?))?;
        kernel
            .start(&PluginId::new(CONFIG_PLUGIN_ID)?)
            .await
            .map_err(|_| "评估配置启动失败；未调用模型。")?;
        let settings = registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID)?)?
            .ok_or("评估配置服务不可用。")?
            .value
            .downcast::<ConfigServiceHandle>()
            .map_err(|_| "评估配置服务类型无效。")?
            .0
            .clone();
        let config = MessageConfig::try_from(&settings.snapshot(MESSAGE_NAMESPACE, 1)?)?;
        let configuration_sha256 = configuration_sha256(settings.as_ref(), options.mode)?;
        kernel.register(Box::new(qq_message_judge::relation_plugin(
            options.mode,
            settings,
        )?))?;
        kernel
            .start(&PluginId::new(RELATION_PLUGIN_ID)?)
            .await
            .map_err(|_| "评估判断服务启动失败。")?;
        let judge = registry
            .get(&ServiceId::new(RELATION_SERVICE_ID)?)?
            .ok_or("评估判断服务不可用。")?
            .value
            .downcast::<RelationServiceHandle>()
            .map_err(|_| "评估判断服务类型无效。")?
            .0
            .clone();
        let mut summary = EvaluationSummary::default();
        let mut cases = Vec::with_capacity(dataset.cases.len());
        for case in dataset.cases {
            let start = Instant::now();
            let result = tokio::time::timeout(
                Duration::from_millis(config.judge_timeout_ms),
                judge.judge(case.input.clone()),
            )
            .await
            .unwrap_or(Err(RelationError::Timeout));
            let result = result.and_then(|decision| {
                decision.validate(&case.input)?;
                Ok(decision)
            });
            let report = score(case, result, start.elapsed(), config.confidence_threshold);
            summary.cases += 1;
            summary.successful_judgements += usize::from(report.error.is_none());
            summary.exact_matches += usize::from(report.exact_match);
            summary.label_matches += usize::from(report.label_match);
            summary.raw_span_matches += usize::from(report.raw_span_match);
            summary.false_cancellations += usize::from(report.false_cancellation);
            summary.correction_required_cases += usize::from(report.correction_required);
            summary.missed_corrections += usize::from(report.missed_correction);
            summary.low_confidence_or_ambiguous += usize::from(report.low_confidence_or_ambiguous);
            summary.errors += usize::from(report.error.is_some());
            summary.timeouts += usize::from(report.error == Some("timeout"));
            summary.mean_latency_ms += report.latency_ms;
            cases.push(report);
        }
        summary.mean_latency_ms /= summary.cases as f64;
        Ok::<_, AppError>(MessageEvaluationReport {
            schema_version: 1,
            started_at_unix_ms,
            mode: match options.mode {
                MessageJudgeMode::Off => "rules",
                MessageJudgeMode::Primary => "primary",
                MessageJudgeMode::Jev => "jev",
            },
            dataset_sha256,
            configuration_sha256,
            judge_timeout_ms: config.judge_timeout_ms,
            confidence_threshold: config.confidence_threshold,
            summary,
            cases,
        })
    }
    .await;
    let stopped = kernel.stop_all().await.map_err(|_| -> AppError {
        "评估插件停止失败，保留本次评估目录。".into()
    });
    let cleaned = if stopped.is_ok() {
        directory.cleanup()
    } else {
        Ok(())
    };
    let report = finish(result, stopped, cleaned)?;
    let bytes = message_evaluation_json(&report)?;
    if let Some(path) = options.output {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| "无法新建评估报告；不会覆盖已有文件。")?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| "写入评估报告失败。")?;
    }
    Ok(report)
}

/// 输出只包含已验证原文、标注和脱敏错误；不输出模型 explanation 或底层诊断。
pub fn message_evaluation_json(report: &MessageEvaluationReport) -> Result<Vec<u8>, AppError> {
    let mut bytes = serde_json::to_vec_pretty(report).map_err(|_| "无法序列化评估报告。")?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REPORT_BYTES {
        return Err("评估报告超过 4 MiB，拒绝写出。".into());
    }
    Ok(bytes)
}

fn finish<T>(
    result: Result<T, AppError>,
    stopped: Result<(), AppError>,
    cleaned: Result<(), AppError>,
) -> Result<T, AppError> {
    let mut secondary: Vec<_> = [stopped, cleaned]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    match result {
        Ok(value) if secondary.is_empty() => Ok(value),
        Ok(_) => Err(Box::new(AppFailure {
            primary: secondary.remove(0),
            secondary,
        })),
        Err(primary) => Err(Box::new(AppFailure { primary, secondary })),
    }
}

fn same_parts(
    actual: &[ObservedPart],
    expected: &[ExpectedPart],
    compare: impl Fn(&ObservedPart, &ExpectedPart) -> bool,
) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let mut matched = vec![false; expected.len()];
    actual.iter().all(|part| {
        if let Some(index) = expected
            .iter()
            .enumerate()
            .position(|(index, candidate)| !matched[index] && compare(part, candidate))
        {
            matched[index] = true;
            true
        } else {
            false
        }
    })
}

fn score(
    case: Case,
    result: Result<RelationDecision, RelationError>,
    elapsed: Duration,
    threshold: u8,
) -> CaseReport {
    let (actual, error) = match result {
        Ok(decision) => (
            decision
                .parts
                .into_iter()
                .map(|part| ObservedPart {
                    intent: part.intent,
                    confidence: part.confidence,
                    span: part.span,
                    raw_text: part
                        .span
                        .map(|span| case.input.message.text[span.start..span.end].to_owned()),
                })
                .collect::<Vec<_>>(),
            None,
        ),
        Err(error) => (
            vec![],
            Some(match error {
                RelationError::Unavailable => "unavailable",
                RelationError::Protocol => "protocol",
                RelationError::Timeout => "timeout",
                RelationError::Panicked => "panicked",
            }),
        ),
    };
    let correction_required = case.accepted.iter().all(|parts| {
        parts
            .iter()
            .any(|part| part.intent == MessageIntent::Correction)
    });
    CaseReport {
        id: case.id,
        exact_match: case.accepted.iter().any(|parts| {
            same_parts(&actual, parts, |a, b| {
                a.intent == b.intent && a.span == b.span
            })
        }),
        label_match: case
            .accepted
            .iter()
            .any(|parts| same_parts(&actual, parts, |a, b| a.intent == b.intent)),
        raw_span_match: case
            .accepted
            .iter()
            .any(|parts| same_parts(&actual, parts, |a, b| a.span == b.span)),
        false_cancellation: actual
            .iter()
            .any(|part| part.intent == MessageIntent::Cancel)
            && case.accepted.iter().all(|parts| {
                parts
                    .iter()
                    .all(|part| part.intent != MessageIntent::Cancel)
            }),
        correction_required,
        missed_correction: correction_required
            && !actual.iter().any(|part| {
                part.intent == MessageIntent::Correction && part.confidence >= threshold
            }),
        low_confidence_or_ambiguous: actual
            .iter()
            .any(|part| part.intent == MessageIntent::Ambiguous || part.confidence < threshold),
        accepted: case.accepted,
        actual,
        latency_ms: elapsed.as_secs_f64() * 1000.0,
        error,
    }
}
