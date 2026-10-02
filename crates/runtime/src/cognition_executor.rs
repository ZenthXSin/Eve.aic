//! 组合层：将目标预算绑定到单次 Session/Control 执行；厂商与插件业务分离。
use crate::{SessionControlRunner, SessionLlmHost};
use eve_cognition_api::{ExecutionAttempt, Goal, Visibility, MAX_RECORDS};
use eve_cognition_loop_api::*;
use eve_control_api::*;
use eve_llm_api::{ExecutionLimits, TurnBudget, TurnEventSink};
use eve_session_api::{SessionInput, SessionKey};
use std::{collections::BTreeMap, sync::{Arc, Mutex}};

#[derive(Clone)]
struct Binding {
    key: SessionKey,
    budget: Arc<TurnBudget>,
}
pub struct BudgetedSessionRunner {
    host: Arc<SessionLlmHost>,
    bindings: Arc<Mutex<BTreeMap<String, Binding>>>,
}
impl BudgetedSessionRunner {
    pub fn new(host: Arc<SessionLlmHost>) -> Self {
        Self { host, bindings: Arc::new(Mutex::new(BTreeMap::new())) }
    }
}
impl ControlRunner for BudgetedSessionRunner {
    fn run<'a>(&'a self, input: SessionInput, sink: &'a dyn TurnEventSink) -> RunFuture<'a> {
        Box::pin(async move {
            let binding = self.bindings.lock().ok()
                .and_then(|bindings| bindings.get(&input.key.session_id).cloned())
                .filter(|binding| binding.key == input.key);
            let Some(binding) = binding else {
                return RunReport {
                    turn_id: None, commit: CommitState::NotStarted, text: None, transcript: None,
                    started_tools: Some(0), tool_results: vec![],
                    failure: Some(RunFailure::Execution(eve_llm_api::LlmError::Configuration(
                        "本次执行没有宿主预算绑定".into()))),
                };
            };
            let host = Arc::new(self.host.with_execution_budget(binding.budget));
            SessionControlRunner::new(host).run(input, sink).await
        })
    }
}
pub struct ControlGoalExecutor {
    control: Arc<dyn ControlService>,
    runner: Arc<BudgetedSessionRunner>,
    internal_user: String,
}
impl ControlGoalExecutor {
    pub fn new(control: Arc<dyn ControlService>, runner: Arc<BudgetedSessionRunner>,
        internal_user: impl Into<String>) -> LoopResult<Self> {
        let internal_user = internal_user.into();
        eve_cognition_api::validate_id(&internal_user)?;
        Ok(Self { control, runner, internal_user })
    }
}
impl GoalExecutor for ControlGoalExecutor {
    fn submit(&self, goal: &Goal, attempt: &ExecutionAttempt) -> LoopResult<GenerationKey> {
        goal.budget.validate()?;
        let user = match &goal.visibility {
            Visibility::User(user) => user.as_str(),
            _ => &self.internal_user,
        };
        let key = SessionKey::new(&attempt.session_id, user).map_err(|_| LoopError::InvalidInput)?;
        let budget = Arc::new(TurnBudget::new(ExecutionLimits {
            max_model_requests: goal.budget.max_model_requests,
            max_tool_calls: goal.budget.max_tool_calls,
            timeout_ms: goal.budget.timeout_ms,
        }).map_err(|_| LoopError::InvalidInput)?);
        {
            let mut bindings = self.runner.bindings.lock().map_err(|_| LoopError::Unavailable)?;
            if bindings.len() >= MAX_RECORDS || bindings.contains_key(&key.session_id) {
                return Err(LoopError::LimitReached);
            }
            bindings.insert(key.session_id.clone(), Binding { key: key.clone(), budget });
        }
        let result = self.control.submit(ControlInput {
            session: SessionInput { key: key.clone(), text: goal.description.clone() },
            task_id: attempt.task_id.clone(),
        }, Arc::new(DiscardControlEvents));
        match result {
            Ok(generation) => Ok(generation),
            Err(_) => {
                self.runner.bindings.lock().map_err(|_| LoopError::Unavailable)?.remove(&key.session_id);
                Err(LoopError::Execution)
            }
        }
    }
    fn cancel(&self, key: &GenerationKey) -> LoopResult<()> {
        self.control.cancel(key).map(|_| ()).map_err(|_| LoopError::Execution)
    }
    fn wait(&self, key: &GenerationKey) -> LoopFuture<'static, GoalExecutionReport> {
        let binding = self.runner.bindings.lock().ok()
            .and_then(|bindings| bindings.get(&key.session.session_id).cloned())
            .filter(|binding| binding.key == key.session);
        let waiting = self.control.wait(key);
        let bindings = self.runner.bindings.clone();
        let session_id = key.session.session_id.clone();
        Box::pin(async move {
            let binding = binding.ok_or(LoopError::Execution)?;
            let result = waiting.await.map_err(|_| LoopError::Execution);
            bindings.lock().map_err(|_| LoopError::Unavailable)?.remove(&session_id);
            Ok(GoalExecutionReport {
                control: result?,
                usage: binding.budget.usage().map_err(|_| LoopError::Execution)?,
            })
        })
    }
}
