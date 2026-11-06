//! 独立 Web 的待办、研究与计划组合，复用已有控制器和持久执行器。
use crate::{AppError, CoreBootstrap, qq_cognition, qq_plan, qq_practice, qq_research};
use eve_cognition_api::{CognitionAdmin, CognitionReader};
use eve_knowledge_api::{KnowledgeAdmin, SourceFetcher, SourcePolicy};
use eve_llm_api::LlmModelResolver;
use eve_plan_api::{CapabilitySpec, PlanJournal, PlanProposer};
use eve_plugin_api::PluginId;
use eve_practice_api::{PracticeAdmin, PracticeRunner};
use std::{
    path::Path,
    sync::{Arc, OnceLock},
};
use tokio::sync::watch;

pub(crate) struct Assembly {
    pub(crate) cognition: Arc<OnceLock<Arc<dyn CognitionAdmin>>>,
    pub(crate) knowledge: Arc<dyn KnowledgeAdmin>,
    pub(crate) practice: Arc<dyn PracticeAdmin>,
    pub(crate) plans: Arc<dyn PlanJournal>,
    goals: Arc<OnceLock<Arc<qq_cognition::Commands>>>,
    commands: Arc<OnceLock<Arc<qq_plan::Commands>>>,
    capabilities: Vec<CapabilitySpec>,
    policy: Option<SourcePolicy>,
    source: Arc<dyn SourceFetcher>,
    runner: Option<Arc<dyn PracticeRunner>>,
}

impl Assembly {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn install(
        kernel: &eve_kernel::Kernel,
        bootstrap: &mut CoreBootstrap,
        admin: Arc<crate::qq_admin::Admin>,
        corpus: Option<&crate::qq_capability_profile::DiscoveredCorpus>,
        source: Arc<dyn SourceFetcher>,
        runner: Option<Arc<dyn PracticeRunner>>,
    ) -> Result<Self, AppError> {
        let plugin = eve_knowledge_plugin::KnowledgePlugin::new()?;
        let knowledge: Arc<dyn KnowledgeAdmin> = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin))?;
        kernel
            .start(&PluginId::new(eve_knowledge_api::KNOWLEDGE_PLUGIN_ID)?)
            .await?;
        let plugin = eve_practice_plugin::PracticePlugin::new()?;
        let practice: Arc<dyn PracticeAdmin> = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin))?;
        kernel
            .start(&PluginId::new(eve_practice_api::PRACTICE_PLUGIN_ID)?)
            .await?;
        let plugin = eve_plan_plugin::PlanPlugin::new("eve")?;
        let plans: Arc<dyn PlanJournal> = Arc::new(plugin.controller());
        kernel.register(Box::new(plugin))?;
        kernel
            .start(&PluginId::new(eve_plan_api::PLAN_PLUGIN_ID)?)
            .await?;

        // 研究入口来自宿主实际发现的资料库；对话不能注入文件系统路径或内网来源。
        let policy = corpus
            .map(|corpus| SourcePolicy::new(std::slice::from_ref(&corpus.seed)))
            .transpose()?;
        let capabilities = qq_plan::capabilities(policy.is_some(), runner.is_some());
        let goals = Arc::new(OnceLock::new());
        let commands = Arc::new(OnceLock::new());
        bootstrap.tools.push(Arc::new(qq_cognition::ManageTool {
            commands: goals.clone(),
            review: crate::qq_natural_goals::Review {
                admin: admin.clone(),
            },
        }));
        if !capabilities.is_empty() {
            bootstrap.tools.push(Arc::new(qq_plan::ManageTool {
                commands: commands.clone(),
                review: crate::qq_natural_goals::Review { admin },
            }));
        }
        Ok(Self {
            cognition: Arc::new(OnceLock::new()),
            knowledge,
            practice,
            plans,
            goals,
            commands,
            capabilities,
            policy,
            source,
            runner,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        &self,
        kernel: &eve_kernel::Kernel,
        backends: &eve_kernel::KernelServices,
        agent: &Path,
        state: &Path,
        admin: Arc<crate::qq_admin::Admin>,
        health: Arc<eve_model_health::HealthBook>,
        resolver: &dyn Fn(&str) -> Arc<dyn LlmModelResolver>,
    ) -> Result<Runtime, AppError> {
        if self.goals.get().is_some()
            || self.cognition.get().is_some()
            || self.commands.get().is_some()
        {
            return Err("Web 待办服务重复启动".into());
        }
        let sessions = backends
            .registry
            .get(&eve_plugin_api::ServiceId::new(
                eve_session_api::SESSION_SERVICE_ID,
            )?)?
            .ok_or("Web 待办会话服务缺失")?
            .value
            .downcast::<eve_session_api::SessionServiceHandle>()
            .map_err(|_| "Web 待办会话服务类型错误")?
            .0
            .clone();
        let cognition = qq_cognition::start(
            kernel,
            backends,
            agent,
            32,
            Arc::new(eve_cognition_loop_plugin::ReflectionPlannerFactory),
            false,
            None,
            health,
        )
        .await?;
        let mut runtime = Runtime {
            cognition,
            plan: None,
            research: None,
            practice: None,
        };
        let result = (|| -> Result<(), AppError> {
            let cognition: Arc<dyn CognitionAdmin> = Arc::new(runtime.cognition.admin()?);
            self.cognition
                .set(cognition.clone())
                .map_err(|_| "Web 对话待办重复绑定")?;
            self.goals
                .set(runtime.cognition.commands.clone())
                .map_err(|_| "Web 自然待办重复绑定")?;
            if !self.capabilities.is_empty() {
                let proposer: Arc<dyn PlanProposer> =
                    Arc::new(eve_plan_plugin::ModelPlanProposer::new(resolver("plan")));
                let background = qq_plan::Background::start(qq_plan::Services {
                    plans: self.plans.clone(),
                    cognition: cognition.clone(),
                    knowledge: self.policy.as_ref().map(|_| self.knowledge.clone()),
                    practice: self.runner.as_ref().map(|_| self.practice.clone()),
                    proposer: proposer.clone(),
                    capabilities: self.capabilities.clone(),
                    authorization: admin.clone(),
                });
                let commands = qq_plan::Commands::enabled(
                    self.plans.clone(),
                    cognition.clone(),
                    sessions,
                    self.capabilities.clone(),
                    proposer.version().into(),
                    background.requests(),
                );
                runtime.plan = Some(background);
                self.commands
                    .set(commands)
                    .map_err(|_| "Web 自然计划重复绑定")?;
            }
            if let Some(policy) = &self.policy {
                let researcher = Arc::new(eve_knowledge_plugin::Researcher::new(
                    self.knowledge.clone(),
                    self.source.clone(),
                    Arc::new(eve_knowledge_plugin::ModelSourceSelector::new(resolver(
                        "research",
                    ))),
                    Arc::new(eve_knowledge_plugin::ModelKnowledgeExtractor::new(
                        resolver("research"),
                    )),
                ));
                runtime.research = Some(qq_research::Background::start(
                    self.knowledge.clone(),
                    cognition.clone(),
                    researcher,
                    policy.clone(),
                    Some(self.plans.clone()),
                    crate::qq_budget::Gate::default(),
                    false,
                    admin.clone(),
                ));
            }
            if let Some(runner) = &self.runner {
                let practitioner = Arc::new(eve_practice_plugin::Practitioner::new(
                    self.practice.clone(),
                    Arc::new(eve_practice_plugin::ModelPracticeDrafter::new(resolver(
                        "practice",
                    ))),
                    runner.clone(),
                ));
                runtime.practice = Some(qq_practice::Background::start(qq_practice::Services {
                    practice: self.practice.clone(),
                    cognition,
                    knowledge: Some(self.knowledge.clone()),
                    practitioner,
                    workspace_root: state.join("plan-practice-work"),
                    skills: None,
                    strategy: None,
                    forging: None,
                    plans: Some(self.plans.clone()),
                    gate: crate::qq_budget::Gate::default(),
                    learning_progress: false,
                    authorization: admin,
                }));
            }
            Ok(())
        })();
        if let Err(error) = result {
            runtime.stop().await?;
            return Err(error);
        }
        Ok(runtime)
    }
}

pub(crate) struct Runtime {
    cognition: qq_cognition::Background,
    plan: Option<qq_plan::Background>,
    research: Option<qq_research::Background>,
    practice: Option<qq_practice::Background>,
}
impl Runtime {
    pub(crate) fn reader(&self) -> Result<Arc<dyn CognitionReader>, AppError> {
        self.cognition.reader()
    }
    pub(crate) fn activate(&self) {
        self.cognition.activate();
        if let Some(plan) = &self.plan {
            plan.activate();
        }
        if let Some(research) = &self.research {
            research.activate();
        }
        if let Some(practice) = &self.practice {
            practice.activate();
        }
    }
    pub(crate) fn finished(&self) -> Vec<watch::Receiver<bool>> {
        let mut result = vec![self.cognition.finished()];
        if let Some(plan) = &self.plan {
            result.push(plan.finished());
        }
        if let Some(research) = &self.research {
            result.push(research.finished());
        }
        if let Some(practice) = &self.practice {
            result.push(practice.finished());
        }
        result
    }
    pub(crate) fn request_stop(&self) {
        self.cognition.request_stop();
        if let Some(plan) = &self.plan {
            plan.request_stop();
        }
        if let Some(research) = &self.research {
            research.request_stop();
        }
        if let Some(practice) = &self.practice {
            practice.request_stop();
        }
    }
    pub(crate) async fn stop(self) -> Result<(), AppError> {
        self.request_stop();
        let (cognition, plan, research, practice) = tokio::join!(
            self.cognition.stop(),
            async {
                match self.plan {
                    Some(plan) => plan.stop().await,
                    None => Ok(()),
                }
            },
            async {
                match self.research {
                    Some(research) => research.stop().await,
                    None => Ok(()),
                }
            },
            async {
                match self.practice {
                    Some(practice) => practice.stop().await,
                    None => Ok(()),
                }
            },
        );
        for result in [cognition, plan, research, practice] {
            result?;
        }
        Ok(())
    }
}
