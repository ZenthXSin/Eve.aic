//! 可信本地主机的记忆只读入口；只经公开插件契约读取，不向模型授予管理能力。
use crate::{AppError, memory_cli_view as view};
use eve_kernel::{Kernel, KernelServices};
use eve_memory_api::{MEMORY_PLUGIN_ID, MemoryAdmin, MemoryScope, validate_id};
use eve_memory_plugin::MemoryPlugin;
use eve_plugin_api::PluginId;
use serde_json::{Value, json};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

pub const MEMORY_CLI_HELP: &str = "Eve 本地记忆只读入口
用法：eve-memory --state-dir 已有目录 [--database-config 凭据文件] 命令
  --channel 通道 --session 会话 --user 用户 status
  --channel 通道 --session 会话 --user 用户 list [--page 1] [--page-size 20]
  --channel 通道 --session 会话 --user 用户 show --id 偏好ID [--page 1] [--page-size 20]
  --channel 通道 --session 会话 --user 用户 evidence --id 证据ID
  scopes [--page 1] [--page-size 20]
list/show/evidence 可加 --include-content，显式显示至多 512 UTF-8 字节的正文预览。
默认只输出标识、来源、计数和有效状态，不输出正文。页大小为 1 至 50。
作用域三个字段必须完整提供；scopes 只供可信本地管理员枚举已有作用域。
命令不调用模型，不修改记忆，不初始化新状态目录；使用现有后端排他锁。
PostgreSQL 只接受已绑定的状态目录与匹配配置；不会自动迁移或清空损坏状态。
输出为 format_version=1 的有界 JSON；正文预览不代表事实核验。";

const MAX_REPORT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
enum MemoryCommand {
    Status,
    List,
    Show { id: String },
    Evidence { id: String },
    Scopes,
}

impl MemoryCommand {
    fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::List => "list",
            Self::Show { .. } => "show",
            Self::Evidence { .. } => "evidence",
            Self::Scopes => "scopes",
        }
    }
}

/// 仅可信本地宿主可构建作用域。没有隐式 owner/default 作用域。
#[derive(Clone, Debug)]
pub struct MemoryCliOptions {
    pub state_directory: PathBuf,
    pub database_config: Option<PathBuf>,
    scope: Option<MemoryScope>,
    command: MemoryCommand,
    page: usize,
    page_size: usize,
    include_content: bool,
}

impl MemoryCliOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut args = args.into_iter();
        let mut directory = None;
        let mut database_config = None;
        let mut command = None;
        let mut include_content = false;
        let mut fields = BTreeMap::<String, String>::new();
        while let Some(arg) = args.next() {
            if arg == "--help" || arg == "-h" {
                return Ok(None);
            }
            let arg = arg
                .into_string()
                .map_err(|_| "记忆命令参数必须为 UTF-8。")?;
            if ["status", "list", "show", "evidence", "scopes"].contains(&arg.as_str()) {
                if command.replace(arg).is_some() {
                    return Err("只能指定一个记忆命令。".into());
                }
                continue;
            }
            if arg == "--include-content" {
                if std::mem::replace(&mut include_content, true) {
                    return Err("记忆参数不得重复。".into());
                }
                continue;
            }
            if ![
                "--state-dir",
                "--database-config",
                "--channel",
                "--session",
                "--user",
                "--id",
                "--page",
                "--page-size",
            ]
            .contains(&arg.as_str())
            {
                return Err("未知记忆命令或参数；使用 --help。".into());
            }
            let value = args.next().ok_or("记忆参数缺少值。")?;
            if value.is_empty() {
                return Err("记忆参数值不能为空。".into());
            }
            if value
                .to_str()
                .is_some_and(|value| value.starts_with("--") || value == "-h")
            {
                return Err("记忆参数缺少值；以 -- 开头的路径请使用绝对路径或 ./ 前缀。".into());
            }
            let duplicate = match arg.as_str() {
                "--state-dir" => directory.replace(PathBuf::from(value)).is_some(),
                "--database-config" => database_config.replace(PathBuf::from(value)).is_some(),
                _ => fields
                    .insert(
                        arg,
                        value.into_string().map_err(|_| "参数值必须为 UTF-8。")?,
                    )
                    .is_some(),
            };
            if duplicate {
                return Err("记忆参数不得重复。".into());
            }
        }
        let state_directory = directory.ok_or("只读记忆命令必须显式提供 --state-dir。")?;
        let command = match command.as_deref().ok_or("缺少记忆命令；使用 --help。")? {
            "status" => MemoryCommand::Status,
            "list" => MemoryCommand::List,
            "show" | "evidence" => {
                let id = fields.remove("--id").ok_or("记忆命令缺少 --id。")?;
                validate_id(&id)?;
                if command.as_deref() == Some("show") {
                    MemoryCommand::Show { id }
                } else {
                    MemoryCommand::Evidence { id }
                }
            }
            "scopes" => MemoryCommand::Scopes,
            _ => unreachable!(),
        };
        let scope = if matches!(command, MemoryCommand::Scopes) {
            None
        } else {
            let scope = MemoryScope {
                channel: fields
                    .remove("--channel")
                    .ok_or("缺少 --channel；必须明确记忆作用域。")?,
                session_id: fields
                    .remove("--session")
                    .ok_or("缺少 --session；必须明确记忆作用域。")?,
                user_id: fields
                    .remove("--user")
                    .ok_or("缺少 --user；必须明确记忆作用域。")?,
            };
            scope.validate()?;
            Some(scope)
        };
        let paged = matches!(
            command,
            MemoryCommand::List | MemoryCommand::Show { .. } | MemoryCommand::Scopes
        );
        let (page, page_size) = if paged {
            let page = positive_number(fields.remove("--page"), 1)?;
            let page_size = positive_number(fields.remove("--page-size"), view::PAGE_SIZE)?;
            if page_size > view::MAX_PAGE_SIZE {
                return Err("页大小必须在 1 至 50。".into());
            }
            (page, page_size)
        } else {
            (1, view::PAGE_SIZE)
        };
        if !fields.is_empty()
            || (include_content && matches!(command, MemoryCommand::Status | MemoryCommand::Scopes))
        {
            return Err("当前记忆命令不接受所给参数。".into());
        }
        Ok(Some(Self {
            state_directory,
            database_config,
            scope,
            command,
            page,
            page_size,
            include_content,
        }))
    }
}

fn positive_number(value: Option<String>, default: usize) -> Result<usize, AppError> {
    let Some(value) = value else {
        return Ok(default);
    };
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("页码与页大小必须为正整数。".into());
    }
    let number = value
        .parse::<usize>()
        .map_err(|_| "页码与页大小必须为正整数。")?;
    if number == 0 {
        return Err("页码与页大小必须为正整数。".into());
    }
    Ok(number)
}

/// 打开现有后端并启动 MemoryPlugin，通过绑定作用域的读取能力构建有界报告。
pub async fn run_memory_cli(options: MemoryCliOptions) -> Result<Value, AppError> {
    let state = crate::storage::open_existing_state_store(
        &options.state_directory,
        options.database_config.as_deref(),
    )?;
    let kernel = Kernel::with_services(KernelServices {
        state,
        ..KernelServices::default()
    });
    let result = async {
        let plugin = MemoryPlugin::new()?;
        let admin = plugin.controller();
        kernel.register(Box::new(plugin))?;
        kernel.start(&PluginId::new(MEMORY_PLUGIN_ID)?).await?;
        let mut report = json!({
            "format_version": 1,
            "command": options.command.name(),
            "content_included": options.include_content,
            "read_only": true,
        });
        let data = if matches!(options.command, MemoryCommand::Scopes) {
            view::scopes(admin.scopes()?, options.page, options.page_size)?
        } else {
            let scope = options.scope.as_ref().ok_or("记忆作用域缺失。")?;
            let snapshot = admin.reader(scope.clone())?.snapshot()?;
            if snapshot.scope != *scope {
                return Err("记忆读取返回了不匹配的作用域。".into());
            }
            report["scope"] = serde_json::to_value(scope)?;
            report["revision"] = json!(snapshot.revision);
            match &options.command {
                MemoryCommand::Status => view::status(&snapshot),
                MemoryCommand::List => view::list(
                    &snapshot,
                    options.page,
                    options.page_size,
                    options.include_content,
                )?,
                MemoryCommand::Show { id } => view::show(
                    &snapshot,
                    id,
                    options.page,
                    options.page_size,
                    options.include_content,
                )?,
                MemoryCommand::Evidence { id } => {
                    view::evidence(&snapshot, id, options.include_content)?
                }
                MemoryCommand::Scopes => unreachable!(),
            }
        };
        report
            .as_object_mut()
            .ok_or("记忆报告结构无效。")?
            .extend(data.as_object().ok_or("记忆投影结构无效。")?.clone());
        if serde_json::to_vec(&report)?.len() > MAX_REPORT_BYTES {
            return Err("记忆报告超出输出上限；请减小页大小。".into());
        }
        Ok(report)
    }
    .await;
    crate::finish_core(&kernel, result).await
}
