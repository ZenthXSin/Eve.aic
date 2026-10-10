//! 本机命令帮助，列表来自实际对话工具注册，不依赖模型对自身能力的推测。
use eve_llm_api::ToolDefinition;
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput};
use std::sync::Arc;

pub(crate) struct Help {
    pub(crate) definitions: Vec<ToolDefinition>,
}
impl Help {
    pub(crate) fn handler(definitions: Vec<ToolDefinition>) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self { definitions })
    }
}
impl QqCommandHandler for Help {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let words: Vec<_> = input.text.split_whitespace().collect();
        if words.first() != Some(&"/help") {
            return Ok(None);
        }
        if words.as_slice() == ["/help", "identity"] {
            return Ok(Some(format!(
                "本会话宿主绑定的操作者标识：{}\n开发工具 operators 可填写这个标识；其他群/会话使用各自独立标识。",
                input.session.user_id
            )));
        }
        if !matches!(words.as_slice(), ["/help"] | ["/help", "tools"]) {
            return Ok(Some(
                "用法：/help 查看常用命令和实际对话工具；/help tools 查看工具说明；/help identity 查看本会话操作者标识。".into(),
            ));
        }
        let names = self
            .definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join("、");
        let mut text = format!(
            "当前实际对话工具（{} 个）：{names}\n",
            self.definitions.len()
        );
        if words.len() == 2 {
            for definition in &self.definitions {
                let description: String = definition.description.chars().take(52).collect();
                text.push_str(&format!("\n{}：{description}", definition.name));
            }
        } else {
            text.push_str("\n/train start|stop|status：主动提问训练\n/self-learning status：自主学习状态\n/segment on|off|reset：分段输出\n/remember 内容、/memories、/recall 关键词：记忆\n/interests：兴趣；/knowledge 兴趣ID：资料\n/practice 兴趣ID：实践证据\n/skills、/tools：已固化技能和检查规则，列表可为空\n/goals、/mind、/plans：目标与计划\n/outreach、/outreach off|on：主动跟进\n/cancel：取消当前任务\n/help tools：实际聊天工具说明\n\n命令对应能力未装配时会明确提示。工具调用以真实结果为准；检查规则不等于任意代码插件。");
        }
        Ok(Some(text))
    }
}
