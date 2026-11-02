//! 独立 Web 宿主；使用相同 Kernel/配置/模型/会话/工具，不读取 QQ 凭据或启动 Node。
use crate::AppError;
use eve_plugin_api::{PluginId, ServiceId};
use std::{ffi::OsString, net::SocketAddr, path::PathBuf, sync::Arc};
pub const WEB_HELP: &str = "用法：eve-web [--listen 环回IP:端口] [--state-dir 目录] [--database-config 文件] [--agent 文件]\n默认 http://127.0.0.1:8765；需要 EVE_WEB_TOKEN（32 至 256 字节可见 ASCII）与 EVE_OPENAI_API_KEY。Web 默认主人身份，不需要 QQBot AppID、密钥、绑定或 Node。模型与开发工具沿用宿主配置。";
#[derive(Clone)]
pub struct WebOptions {
    pub listen: SocketAddr,
    pub state_directory: PathBuf,
    pub database_config: Option<PathBuf>,
    pub agent_path: PathBuf,
}
impl Default for WebOptions {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8765".parse().expect("固定地址"),
            state_directory: ".eve".into(),
            database_config: None,
            agent_path: "AGENT.md".into(),
        }
    }
}
impl WebOptions {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, AppError> {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            if matches!(arg.to_str(), Some("--help" | "-h")) {
                return Ok(None);
            }
            let value = args.next().ok_or("Web 启动参数缺少值")?;
            match arg.to_str() {
                Some("--listen" | "--web-listen") => {
                    options.listen = value.to_str().ok_or("Web 地址无效")?.parse()?
                }
                Some("--state-dir") => options.state_directory = value.into(),
                Some("--database-config") => options.database_config = Some(value.into()),
                Some("--agent") => options.agent_path = value.into(),
                _ => return Err("未知 Web 启动参数；使用 --help 查看".into()),
            }
        }
        if !options.listen.ip().is_loopback() {
            return Err("Web 面板须绑定环回地址".into());
        }
        Ok(Some(options))
    }
}
pub async fn run_web(options: WebOptions) -> Result<(), AppError> {
    let token = std::env::var("EVE_WEB_TOKEN").map_err(|_| "开启 Web 需要 EVE_WEB_TOKEN")?;
    let config = eve_web_panel::PanelConfig {
        address: options.listen,
        token,
    };
    config.validate()?;
    let mut bootstrap = crate::core_bootstrap(&options.agent_path)?;
    let backends = eve_kernel::KernelServices {
        state: crate::storage::open_state_store(
            &options.state_directory,
            options.database_config.as_deref(),
        )?,
        ..Default::default()
    };
    let registry = backends.registry.clone();
    let permissions = backends.permissions.clone();
    let logger = backends.logger.clone();
    let kernel = eve_kernel::Kernel::with_services(backends);
    let health = Arc::new(eve_model_health::HealthBook::default());
    bootstrap.health = Some(health.clone());
    let result = async {
        let permit = eve_web_panel_api::PageWritePermit::default();
        let reviewer = crate::developer::action_reviewer(registry.clone(), health.clone());
        crate::developer::install_with_member_runtime(
            &mut bootstrap,
            &options.state_directory,
            Some(Arc::new(crate::web_chat::OwnerAuthorization)),
            None,
            reviewer,
        )?;
        let plugin = eve_context_plugin::ContextPlugin::new(Arc::new(
            eve_context_plugin::FileContextArchive::open(
                options.state_directory.join("context-notes-archive"),
            )?,
        ))?;
        let notes: Arc<dyn eve_context_api::ContextAdmin> = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin))?;
        kernel
            .start(&PluginId::new(eve_context_api::CONTEXT_PLUGIN_ID)?)
            .await?;
        bootstrap
            .tools
            .push(Arc::new(crate::context_notes::NotesTool {
                admin: notes.clone(),
                binding: crate::context_notes::NoteBinding::Web,
            }));
        bootstrap.context = Some(Arc::new(crate::web_chat::WebContext {
            inner: Arc::new(crate::context_notes::NotesContext {
                channel: "web",
                inner: bootstrap
                    .context
                    .take()
                    .unwrap_or_else(crate::services::default_context),
                admin: notes.clone(),
            }),
            web: None,
        }));
        let tools_registered = bootstrap.tools.len() + 1;
        let (control, _) = crate::install_core_with_pages_and_admin(
            &kernel,
            registry.clone(),
            permissions,
            logger,
            &options.state_directory,
            bootstrap,
            Some(permit.clone()),
        )
        .await?;
        let sessions = registry
            .get(&ServiceId::new(eve_session_api::SESSION_SERVICE_ID)?)?
            .ok_or("Web 会话服务缺失")?
            .value
            .downcast::<eve_session_api::SessionServiceHandle>()
            .map_err(|_| "Web 会话服务类型错误")?
            .0
            .clone();
        let settings = crate::qq_tasks::config_service(registry.as_ref())?;
        let chat = Arc::new(
            crate::web_chat::WebChat::open(control.clone(), &options.state_directory)
                .map_err(|_| "Web 请求回执目录不可用；保留原记录")?,
        );
        let panel = eve_web_panel::LocalPanel::bind(
            config,
            Arc::new(crate::web_panel::QqPanel {
                notes,
                sessions,
                control,
                channel: None,
                chat: Some(chat.clone()),
                started_at_unix_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis()
                    .try_into()?,
                judgments: None,
                cognition: None,
                memory: None,
                learning: None,
                autonomy: None,
                plans: None,
                plugins: crate::web_panel_plugins::PanelPlugins::new(
                    Arc::new(kernel.clone()),
                    Arc::new(kernel.clone()),
                    registry.clone(),
                    permit,
                    Default::default(),
                )
                .map_err(|_| "Web 插件管理初始化失败")?,
                readiness: crate::web_panel::PanelReadiness {
                    health,
                    settings,
                    tools_registered,
                },
            }),
        )
        .await?;
        eprintln!("EVE_WEB_READY http://{}", panel.address());
        let mut finished = panel.finished();
        let outcome: Result<(), AppError> = tokio::select! {
            result = shutdown_signal() => result.map_err(Into::into),
            _ = finished.changed() => Err("Web 监听异常结束；已停止准入，保留原记录".into()),
        };
        panel.request_stop();
        let stopped = panel.stop().await;
        let chats = chat
            .shutdown()
            .await
            .map_err(|_| "Web 在途任务收尾未能确认；原记录保留");
        outcome?;
        stopped?;
        chats?;
        Ok(())
    }
    .await;
    crate::finish_core(&kernel, result).await
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result, _ = terminate.recv() => Ok(()) }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
