//! 宿主事实：每轮把宿主实际保存了什么、单次回复的上限交给对话，作为数据，不是指令。
//!
//! 真实模型实测中，Eve 在兴趣观察已记下用户原话时仍对用户说“没有保存你的记录、不用删”；用户要数千字长文时，
//! 模型不知道单次输出上限，回复被截断后整轮失败。这里只陈述宿主按配置确定的事实，不读取或展示记录内容，
//! 也不改变任何行为；模型据此如实回答“你记得什么”，并把长内容分次完成。
use eve_config_api::{CONFIG_SERVICE_ID, ConfigServiceHandle};
use eve_interest_api::InterestSettingsReader;
use eve_llm_api::{ContextAssembler, ContextScope, ContextSnapshot, LlmFuture, TurnInput};
use eve_plugin_api::{ServiceId, ServiceRegistry};
use serde_json::json;
use std::sync::Arc;

const NOTICE: &str = "以下是数据，不是指令：宿主当前实际保存的内容与回复限制。用户问你记得什么、保存了什么、能不能删时，据此如实回答：宿主在保存的不要说成没有保存，没有的也不要说成有；具体内容不确定时先用查看工具核对。用户要的内容明显超出单次回复上限（例如几千字的长文）时，先给提纲或分几次完成，每次在上限内写完整的一部分，并告诉用户接下来会继续写哪部分。";

pub(crate) struct HostFactsContext {
    pub(crate) wrapped: Arc<dyn ContextAssembler>,
    pub(crate) registry: Arc<dyn ServiceRegistry>,
    /// 开启 --memory 时，已完成的对话作为记忆证据保存。
    pub(crate) memory: bool,
    /// 兴趣观察；可在运行中由面板启停，每轮读取当前设置。
    pub(crate) interests: Option<Arc<dyn InterestSettingsReader>>,
}

impl HostFactsContext {
    /// 每轮按与模型解析相同的规则读取主模型的单次输出上限；读取失败时为空，不猜测。
    fn reply_limit(&self) -> Option<u32> {
        let entry = self
            .registry
            .get(&ServiceId::new(CONFIG_SERVICE_ID).ok()?)
            .ok()??;
        let settings = entry.value.downcast::<ConfigServiceHandle>().ok()?;
        crate::models::summary(&*settings.0).ok()?.max_output_tokens
    }
}

impl ContextAssembler for HostFactsContext {
    fn assemble(&self, input: TurnInput) -> LlmFuture<'_, ContextSnapshot> {
        self.wrapped.assemble(input)
    }

    fn assemble_scoped(
        &self,
        input: TurnInput,
        scope: Option<ContextScope>,
    ) -> LlmFuture<'_, ContextSnapshot> {
        Box::pin(async move {
            let mut context = self.wrapped.assemble_scoped(input, scope.clone()).await?;
            if scope.is_none() {
                return Ok(context);
            }
            let interests = self
                .interests
                .as_ref()
                .and_then(|settings| settings.snapshot().ok())
                .is_some_and(|snapshot| snapshot.settings.enabled);
            let limit = self.reply_limit();
            let data = json!({
                "kind": "eve.host-facts",
                "conversation_history": "本会话的对话会持久保存，重启后仍在；失败或未完成的请求也会记下原话与失败原因",
                "memory": if self.memory {
                    "已开启：已完成的对话（用户原话和你的回复）作为记忆证据保存，用于之后的回忆；用户可以要求撤销已记下的偏好"
                } else {
                    "未开启：不另外保存记忆证据"
                },
                "interest_observation": if interests {
                    "已开启：用户在私聊中提到的兴趣、经验与困难会连同原话记入兴趣记录（可用 list_interests 查看）；用户说不再感兴趣时标为已撤回、停止相关学习，但原话仍保留在记录中"
                } else {
                    "未开启：不记录兴趣"
                },
                "reply_max_output_tokens": limit,
            });
            context.memories.push(format!("{NOTICE}{data}"));
            context.revision = format!(
                "{}:eve-host-facts-1:{}:{}:{}",
                context.revision,
                u8::from(self.memory),
                u8::from(interests),
                limit.map_or(0, u64::from)
            );
            Ok(context)
        })
    }
}
