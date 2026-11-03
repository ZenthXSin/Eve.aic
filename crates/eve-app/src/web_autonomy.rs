//! 独立 Web 的业务组合；复用任务、主体、技能和条件调度，不创建 QQ 身份或发送路由。
use crate::{AppError, CoreBootstrap};
use eve_llm_api::{ContextAssembler, LlmModelResolver};
use eve_plugin_api::{PermissionChecker, PluginId, ServiceRegistry};
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};
use tokio::sync::watch;

pub(crate) struct WebAutonomy {
    pub admin: Arc<crate::qq_admin::Admin>,
    pub business: Arc<crate::qq_business::BusinessTool>,
    pub tasks: Arc<dyn eve_task_api::TaskAdmin>,
    pub triggers: Arc<dyn eve_trigger_api::TriggerAdmin>,
    pub memory: Arc<dyn eve_memory_api::MemoryAdmin>,
    pub training: Arc<dyn eve_training_api::TrainingService>,
    subject: Arc<dyn eve_subject_api::SubjectAdmin>,
    pub admission: Arc<crate::qq_subject_admission::Admission>,
    late: Arc<crate::qq_tasks::Late>,
    observations: Arc<OnceLock<Arc<crate::qq_triggers::Observations>>>,
    development: Option<eve_developer_api::DeveloperObservationCapabilities>,
    conversations: eve_qqbot_plugin::QqConversations,
    digest_policy: Arc<crate::qq_history_digest::Policy>,
    digests: Arc<eve_history_digest::HistoryDigestService>,
    reasoning: Arc<OnceLock<Arc<crate::qq_model::Commands>>>,
    worker_tools: Vec<String>,
    worker_config: eve_runtime::LlmHostConfig,
    api_key: String,
}

impl WebAutonomy {
    pub(crate) async fn install(
        kernel: &eve_kernel::Kernel,
        registry: Arc<dyn ServiceRegistry>,
        state_store: Arc<dyn eve_plugin_api::StateStore>,
        bootstrap: &mut CoreBootstrap,
        state: &Path,
        health: Arc<eve_model_health::HealthBook>,
    ) -> Result<Self, AppError> {
        kernel.register(Box::new(
            eve_training_plugin::TrainingPlugin::new(true)?
                .with_phrase_learning()
                .with_expression_store(Arc::new(eve_training_sqlite::SqliteExpressions::open(
                    &state.join("training-expressions.sqlite3"),
                )?)),
        ))?;
        kernel
            .start(&PluginId::new(eve_training_api::TRAINING_PLUGIN_ID)?)
            .await?;
        let training = registry
            .get(&eve_plugin_api::ServiceId::new(
                eve_training_api::TRAINING_SERVICE_ID,
            )?)?
            .ok_or("Web 交流训练服务缺失")?
            .value
            .downcast::<eve_training_api::TrainingServiceHandle>()
            .map_err(|_| "Web 交流训练服务类型错误")?
            .0
            .clone();
        bootstrap.context = Some(Arc::new(crate::conversation_learning::Context {
            inner: bootstrap
                .context
                .take()
                .unwrap_or_else(crate::services::default_context),
            training: training.clone(),
        }));
        bootstrap
            .tools
            .push(Arc::new(crate::conversation_learning::Manage {
                training: training.clone(),
            }));
        let mut roots = vec![std::env::current_dir()?];
        if let Some(directory) = std::env::current_exe()?.parent()
            && !roots.iter().any(|root| root == directory)
        {
            roots.push(directory.to_path_buf());
        }
        let resources = crate::qq_capability_profile::discover_resources(
            &roots,
            std::env::var_os("PATH").as_deref(),
        );
        let practice_runner: Option<Arc<dyn eve_practice_api::PracticeRunner>> = resources
            .browser
            .as_ref()
            .map(|browser| {
                eve_practice_browser::BrowserRunner::new(browser.clone())
                    .map(|runner| Arc::new(runner) as Arc<dyn eve_practice_api::PracticeRunner>)
            })
            .transpose()?;
        let runner = practice_runner.map(|runner| {
            let slot = Arc::new(OnceLock::new());
            let _ = slot.set(runner);
            slot
        });
        let conversations = eve_qqbot_plugin::QqConversations::default();
        let admin = crate::qq_admin::Admin::open(
            state,
            conversations.clone(),
            Default::default(),
            registry.clone(),
        )?;
        let subject_plugin = eve_subject_plugin::SubjectPlugin::new()?;
        let subject: Arc<dyn eve_subject_api::SubjectAdmin> = Arc::new(subject_plugin.controller());
        kernel.register(Box::new(subject_plugin))?;
        kernel
            .start(&PluginId::new(eve_subject_api::SUBJECT_PLUGIN_ID)?)
            .await?;
        subject.abandon_pending_kind("work_admission", crate::qq_tasks::now_ms())?;
        let admission = Arc::new(crate::qq_subject_admission::Admission(subject.clone()));

        let task_plugin = eve_task_plugin::TaskPlugin::new()?.with_archive(Arc::new(
            eve_task_plugin::FileTaskArchive::open(state.join("task-archive"))?,
        ));
        let tasks: Arc<dyn eve_task_api::TaskAdmin> = Arc::new(task_plugin.controller());
        kernel.register(Box::new(task_plugin))?;
        kernel
            .start(&PluginId::new(eve_task_api::TASK_PLUGIN_ID)?)
            .await?;
        let trigger_plugin = eve_trigger_plugin::TriggerPlugin::new()?;
        let triggers: Arc<dyn eve_trigger_api::TriggerAdmin> =
            Arc::new(trigger_plugin.controller());
        kernel.register(Box::new(trigger_plugin))?;
        kernel
            .start(&PluginId::new(eve_trigger_api::PLUGIN_ID)?)
            .await?;
        let business = Arc::new(crate::qq_business::BusinessTool {
            admin: admin.clone(),
            reviewer: crate::developer::action_reviewer(registry.clone(), health.clone()),
            tasks: Some(tasks.clone()),
            subject: subject.clone(),
            triggers: Some(triggers.clone()),
        });
        business.recover_pending()?;
        bootstrap.tools.push(business.clone());
        let developer = crate::developer::install_with_member_runtime_backend_profile(
            bootstrap,
            state,
            Some(admin.clone()),
            None,
            crate::developer::action_reviewer(registry.clone(), health.clone()),
            true,
        )?;
        let development = developer
            .as_ref()
            .and_then(|backend| backend.observation_capabilities());

        let memory_plugin = eve_memory_plugin::MemoryPlugin::new()?.with_archive(
            Arc::new(eve_memory_plugin::FileEvidenceArchive::open(
                crate::storage::memory_archive_directory(state),
            )?),
            eve_memory_plugin::ArchivePolicy::default(),
        )?;
        let memory: Arc<dyn eve_memory_api::MemoryAdmin> = Arc::new(memory_plugin.controller());
        kernel.register(Box::new(memory_plugin))?;
        kernel
            .start(&PluginId::new(eve_memory_api::MEMORY_PLUGIN_ID)?)
            .await?;
        let experience_plugin = eve_memory_plugin::ExperienceMemoryPlugin::new()?;
        let experience = Arc::new(experience_plugin.controller());
        kernel.register(Box::new(experience_plugin))?;
        kernel
            .start(&PluginId::new(eve_memory_api::EXPERIENCE_MEMORY_PLUGIN_ID)?)
            .await?;
        let skill_plugin = eve_skill_plugin::SkillPlugin::new()?;
        let skills: Arc<dyn eve_skill_api::SkillAdmin> = Arc::new(skill_plugin.controller());
        kernel.register(Box::new(skill_plugin))?;
        kernel
            .start(&PluginId::new(eve_skill_api::SKILL_PLUGIN_ID)?)
            .await?;
        let forge_plugin = eve_toolforge_plugin::ToolForgePlugin::new()?;
        let checks: Arc<dyn eve_toolforge_api::ToolAdmin> = Arc::new(forge_plugin.controller());
        kernel.register(Box::new(forge_plugin))?;
        kernel
            .start(&PluginId::new(eve_toolforge_api::TOOLFORGE_PLUGIN_ID)?)
            .await?;
        let web_fetcher: Arc<dyn eve_knowledge_api::SourceFetcher> =
            Arc::new(eve_knowledge_plugin::HttpSourceFetcher::new()?);
        let source: Arc<dyn eve_knowledge_api::SourceFetcher> = match &resources.corpus {
            Some(corpus) => Arc::new(eve_knowledge_plugin::RoutingSourceFetcher::new(
                Some(Arc::new(eve_knowledge_plugin::CorpusSourceFetcher::open(
                    &corpus.directory,
                )?)),
                web_fetcher,
            )),
            None => web_fetcher,
        };
        let source_seeds: Vec<_> = resources
            .corpus
            .as_ref()
            .map(|corpus| corpus.seed.clone())
            .into_iter()
            .collect();
        let source_policy = if source_seeds.is_empty() {
            eve_knowledge_api::SourcePolicy::unrestricted_web()
        } else {
            eve_knowledge_api::SourcePolicy::new(&source_seeds)?.with_unrestricted_web()
        }
        .with_public_web_only();
        let (tools, preferences) = eve_dialogue_tools::Services {
            channel: Some("web".into()),
            memory: Some(memory.clone()),
            skills: Some(skills.clone()),
            checks: Some(checks.clone()),
            runner: runner.clone(),
            source: Some((source_policy, source)),
            workspace_root: state.join("dialogue-practice-work"),
            owner_check: Some(Arc::new(eve_dialogue_tools::OwnerCheck({
                let admin = admin.clone();
                move |scope: &eve_llm_api::ContextScope| admin.is_owner(scope)
            }))),
            ..Default::default()
        }
        .tools();
        bootstrap.tools.extend(tools);
        if let Some(runner) = &runner {
            bootstrap
                .tools
                .push(Arc::new(eve_skill_plugin::SkillTool::new(
                    skills.clone(),
                    runner.clone(),
                    state.join("skill-invocations"),
                )));
        }
        if let Some(services) = preferences {
            bootstrap
                .tools
                .push(Arc::new(eve_dialogue_tools::SavePreferenceTool {
                    services,
                }));
        }
        bootstrap.tools.push(Arc::new(crate::qq_skill::ManageTool {
            skills: skills.clone(),
        }));
        bootstrap
            .tools
            .push(Arc::new(crate::qq_tools::ManageTool { tools: checks }));
        bootstrap
            .tools
            .extend(crate::subject_memory::tools_with_principal_resolver(
                "web",
                experience.clone(),
                None,
            )?);
        let mut context: Arc<dyn ContextAssembler> = bootstrap
            .context
            .take()
            .unwrap_or_else(crate::services::default_context);
        context = Arc::new(eve_memory_plugin::MemoryContext::new(
            "web",
            memory.clone(),
            context,
        )?);
        context = Arc::new(crate::subject_memory::ExperienceContext::new(
            "web", experience, context,
        )?);
        context = Arc::new(crate::qq_skill::SkillContext {
            wrapped: context,
            skills,
        });
        context = Arc::new(crate::qq_tasks::TaskContext {
            wrapped: context,
            tasks: tasks.clone(),
        });
        context = Arc::new(crate::qq_subject::Context {
            inner: context,
            subject: subject.clone(),
        });
        context = Arc::new(crate::qq_admin::AccessContext {
            inner: context,
            admin: admin.clone(),
        });
        bootstrap.context = Some(context);

        let late = Arc::new(crate::qq_tasks::Late::default());
        late.admission
            .set(admission.clone())
            .map_err(|_| "Web 主体准入重复装配")?;
        late.profiles
            .set(Arc::new(
                eve_subagent_profile_sqlite::SqliteSubagentProfiles::open(
                    &state.join("subagent-profiles.sqlite3"),
                )?,
            ))
            .map_err(|_| "Web 子代理档案重复装配")?;
        bootstrap
            .tools
            .push(Arc::new(crate::qq_tasks::DispatchTool {
                tasks: tasks.clone(),
                late: late.clone(),
            }));
        bootstrap.tools.push(Arc::new(crate::qq_tasks::ListTool {
            tasks: tasks.clone(),
        }));
        bootstrap.tools.push(Arc::new(crate::qq_tasks::CancelTool {
            tasks: tasks.clone(),
        }));
        bootstrap
            .tools
            .push(Arc::new(crate::qq_tasks::SubagentTool {
                late: late.clone(),
            }));
        for kind in 0..3 {
            bootstrap
                .tools
                .push(Arc::new(crate::qq_subagents::ProfileTool {
                    late: late.clone(),
                    kind,
                }));
        }
        let observations = Arc::new(OnceLock::new());
        for operation in [
            "set_wakeup_trigger",
            "list_wakeup_triggers",
            "cancel_wakeup_trigger",
        ] {
            bootstrap.tools.push(Arc::new(crate::qq_triggers::Tools {
                admin: triggers.clone(),
                observations: observations.clone(),
                conversations: conversations.clone(),
                operation,
            }));
        }
        bootstrap
            .tools
            .push(Arc::new(crate::qq_task_wait::WaitTool {
                tasks: tasks.clone(),
                triggers: triggers.clone(),
                observations: observations.clone(),
            }));
        bootstrap
            .foreground_excluded_tools
            .insert(crate::qq_task_wait::TOOL_NAME.into());
        bootstrap
            .tools
            .push(Arc::new(crate::qq_subject::FocusTool(subject.clone())));
        bootstrap
            .tools
            .push(Arc::new(crate::qq_subject::ConfigureTool {
                subject: subject.clone(),
                owner: admin.clone(),
                reviewer: crate::developer::action_reviewer(registry.clone(), health),
            }));
        bootstrap
            .tools
            .push(Arc::new(eve_dialogue_tools::SleepTool));
        bootstrap
            .tools
            .push(Arc::new(eve_dialogue_tools::TaskProgressTool));
        let reasoning = Arc::new(OnceLock::new());
        bootstrap
            .tools
            .push(Arc::new(crate::qq_model::ReasoningTool(reasoning.clone())));
        bootstrap
            .tools
            .push(Arc::new(crate::qq_model_status::StatusTool(
                reasoning.clone(),
            )));
        let workflows = eve_workflow_tools::WorkflowStore::open(state.join("workflow-tools"))?;
        bootstrap
            .tools
            .extend(eve_workflow_tools::tools(workflows.clone()));
        let python = std::env::var_os("EVE_SCRIPT_PYTHON")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| "/usr/bin/python3".into());
        let node = std::env::var_os("EVE_SCRIPT_NODE")
            .map(std::path::PathBuf::from)
            .or_else(|| std::fs::canonicalize("/usr/bin/node").ok())
            .or_else(|| std::fs::canonicalize("/usr/local/bin/node").ok())
            .unwrap_or_else(|| "/usr/bin/node".into());
        let mut sandbox = eve_script_runtime::Bubblewrap::new(python, node);
        if let Some(rustc) = std::env::var_os("EVE_SCRIPT_RUSTC") {
            sandbox = sandbox.with_rustc(rustc);
        }
        match (
            std::env::var_os("EVE_SCRIPT_CONTAINER_CLI"),
            std::env::var("EVE_SCRIPT_CONTAINER_IMAGE").ok(),
        ) {
            (Some(cli), Some(image)) => {
                sandbox = sandbox.with_container(eve_script_runtime::ContainerSandbox {
                    cli: cli.into(),
                    image,
                })
            }
            (None, None) => {}
            _ => {
                return Err(
                    "容器沙箱需要同时配置 EVE_SCRIPT_CONTAINER_CLI 与 EVE_SCRIPT_CONTAINER_IMAGE"
                        .into(),
                );
            }
        }
        sandbox = crate::qq_scripts::configure_cargo(sandbox)?;
        let scripts = crate::qq_scripts::ScriptTools::open(
            state.join("script-plugins"),
            sandbox,
            Arc::new(crate::qq_access::ScriptAuthorizer {
                admin: admin.clone(),
                public_github: false,
                developer,
                outbox: None,
            }),
        )?;
        bootstrap.tools.extend(scripts.tools());
        let (script_store, sandbox) = scripts.replay_source();
        bootstrap.tools.extend(crate::qq_failure_replay::tools(
            script_store,
            sandbox,
            state_store,
            tasks.clone(),
        ));
        let context_planner = Arc::new(crate::layered_context::Planner::new(
            registry,
            Some(tasks.clone()),
        ));
        bootstrap.history_context_planner = Some(context_planner.clone());
        bootstrap
            .tools
            .push(Arc::new(crate::layered_context::ReadRecord(
                context_planner,
            )));
        crate::qq_capability_profile::install_scoped_report_tool(
            &mut bootstrap.tools,
            runner.is_some(),
            resources.corpus.is_some(),
            resources.unavailable,
            &bootstrap.foreground_excluded_tools,
        );
        for tool in &mut bootstrap.tools {
            *tool = Arc::new(crate::qq_access::GuardedTool {
                inner: tool.clone(),
                admin: admin.clone(),
                public_github: false,
            });
        }
        workflows.bind_tools(
            bootstrap
                .tools
                .iter()
                .filter(|tool| {
                    !eve_workflow_tools::TOOL_NAMES.contains(&tool.definition().name.as_str())
                })
                .cloned()
                .collect(),
        )?;
        scripts.bind_tools(
            bootstrap
                .tools
                .iter()
                .filter(|tool| crate::qq_scripts::is_synchronous_leaf(&tool.definition().name))
                .cloned()
                .collect(),
        )?;
        bootstrap.context = Some(Arc::new(eve_dialogue_tools::CapabilitiesContext {
            inner: bootstrap.context.take().ok_or("Web 业务上下文缺失")?,
            definitions: std::iter::once(crate::services::echo_definition())
                .chain(
                    bootstrap
                        .tools
                        .iter()
                        .filter(|tool| {
                            !bootstrap
                                .foreground_excluded_tools
                                .contains(&tool.definition().name)
                        })
                        .map(|tool| tool.definition()),
                )
                .collect(),
            source_seeds,
            runner,
        }));
        let digest_policy = Arc::new(crate::qq_history_digest::Policy::default());
        let digests = Arc::new(eve_history_digest::HistoryDigestService::new(
            eve_history_digest::DigestStore::open(state.join("history-digest"))?,
            Box::new(crate::qq_history_digest::SharedPolicy(
                digest_policy.clone(),
            )),
        ));
        bootstrap.history_digests = Some(digests.clone());
        bootstrap.host_config.max_tool_rounds = 0;
        bootstrap.host_config.max_tool_calls = 100;
        bootstrap.host_config.final_answer_on_round_limit = true;
        bootstrap.host_config.tool_progress_reports = true;
        bootstrap
            .host_config
            .step_bounded_tools
            .insert(crate::qq_tasks::SUBAGENT_TOOL.into());
        bootstrap
            .host_config
            .tool_timeout_overrides
            .insert("sleep".into(), std::time::Duration::from_secs(305));
        Ok(Self {
            admin,
            business,
            tasks,
            triggers,
            memory,
            training,
            subject,
            admission,
            late,
            observations,
            development,
            conversations,
            digest_policy,
            digests,
            reasoning,
            worker_tools: bootstrap
                .tools
                .iter()
                .map(|tool| tool.definition().name)
                .collect(),
            worker_config: bootstrap.host_config.clone(),
            api_key: bootstrap.api_key.clone(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        &self,
        kernel: &eve_kernel::Kernel,
        registry: Arc<dyn ServiceRegistry>,
        permissions: Arc<dyn PermissionChecker>,
        control: Arc<dyn eve_control_api::ControlService>,
        sessions: Arc<dyn eve_session_api::SessionService>,
        settings_admin: eve_config_plugin::ConfigController,
        health: Arc<eve_model_health::HealthBook>,
    ) -> Result<WebRuntime, AppError> {
        let settings = crate::qq_tasks::config_service(registry.as_ref())?;
        self.late
            .config
            .set(settings.clone())
            .map_err(|_| "Web 后台配置重复装配")?;
        self.digest_policy
            .settings
            .set(settings.clone())
            .map_err(|_| "Web 摘要配置重复装配")?;
        self.reasoning
            .set(Arc::new(crate::qq_model::Commands {
                settings: settings.clone(),
                admin: Arc::new(settings_admin),
                authorization: self.admin.clone(),
            }))
            .map_err(|_| "Web 模型管理重复装配")?;
        let resolver = |role: &str| -> Arc<dyn LlmModelResolver> {
            health.register(role);
            Arc::new(eve_model_health::ObservedResolver::new(
                Arc::new(
                    crate::models::CoreModelResolver::new(settings.clone(), self.api_key.clone())
                        .with_use(role),
                ),
                role,
                health.clone(),
            ))
        };
        let worker = crate::qq_tasks::Worker::install(
            kernel,
            registry,
            permissions,
            &self.worker_tools,
            self.worker_config.clone(),
            resolver(crate::qq_tasks::MODEL_USE),
            self.digests.clone(),
        )
        .await?;
        worker.authorize_with(self.admin.clone());
        self.late
            .worker
            .set(worker)
            .map_err(|_| "Web 后台执行器重复装配")?;
        let observations = Arc::new(
            crate::qq_triggers::Observations::new(sessions.clone(), Some(self.tasks.clone()))?
                .with_development(self.development.clone())
                .with_authorization(self.admin.clone())
                .with_admission(self.admission.clone()),
        );
        self.observations
            .set(observations.clone())
            .map_err(|_| "Web 条件观察重复装配")?;
        let digester = Arc::new(eve_history_digest::ModelHistoryDigester::new(resolver(
            crate::qq_history_digest::MODEL_USE,
        )));
        self.digests.install_compactor(digester.clone())?;
        let recovery = Arc::new(crate::qq_failure_recovery::Recovery {
            tasks: self.tasks.clone(),
            sessions: sessions.clone(),
            settings: settings.clone(),
            authorization: self.admin.clone(),
            digests: self.digests.clone(),
        });
        Ok(WebRuntime {
            conversation: crate::conversation_learning::Background::start(
                self.training.clone(),
                Arc::new(eve_training_plugin::ModelConversationLearner::new(
                    resolver("conversation_learning"),
                )),
                crate::qq_budget::Gate::default(),
            ),
            recovery,
            task: crate::qq_tasks::Background::start(self.tasks.clone(), self.late.clone()),
            trigger: crate::qq_triggers::Background::start(
                self.triggers.clone(),
                observations,
                control.clone(),
                self.conversations.clone(),
            ),
            subject: crate::qq_subject::Background::start_with_services(
                self.subject.clone(),
                self.tasks.clone(),
                self.admin.clone(),
                Some(sessions),
                Some(control),
            ),
            digest: crate::qq_history_digest::Background::start(
                self.digests.clone(),
                digester,
                self.digest_policy.clone(),
            ),
        })
    }
}

pub(crate) struct WebRuntime {
    pub recovery: Arc<crate::qq_failure_recovery::Recovery>,
    task: crate::qq_tasks::Background,
    trigger: crate::qq_triggers::Background,
    subject: crate::qq_subject::Background,
    digest: crate::qq_history_digest::Background,
    conversation: crate::conversation_learning::Background,
}
impl WebRuntime {
    pub(crate) fn activate(&self) -> Result<(), AppError> {
        self.subject.activate()?;
        self.task.activate();
        self.trigger.activate();
        self.digest.activate();
        self.conversation.activate();
        Ok(())
    }
    pub(crate) fn finished(&self) -> Vec<watch::Receiver<bool>> {
        vec![
            self.task.finished(),
            self.trigger.finished(),
            self.subject.finished(),
            self.digest.finished(),
            self.conversation.finished(),
        ]
    }
    pub(crate) fn request_stop(&self) {
        self.task.request_stop();
        self.trigger.request_stop();
        self.subject.request_stop();
        self.digest.request_stop();
        self.conversation.request_stop();
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        let (task, trigger, subject, digest, conversation) = tokio::join!(
            self.task.stop(),
            self.trigger.stop(),
            self.subject.stop(),
            self.digest.stop(),
            self.conversation.stop()
        );
        for result in [task, trigger, subject, digest, conversation] {
            result?;
        }
        Ok(())
    }
}
