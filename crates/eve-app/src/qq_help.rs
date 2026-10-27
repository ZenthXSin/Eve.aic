//! 本机命令帮助，列表来自实际对话工具注册，不依赖模型对自身能力的推测。
use eve_llm_api::ToolDefinition;
use eve_plugin_api::PluginResult;
use eve_qqbot_plugin::{QqCommandHandler, QqCommandInput, QqConversations};
use std::sync::Arc;

pub(crate) struct Help {
    pub(crate) definitions: Vec<ToolDefinition>,
    pub(crate) conversations: QqConversations,
}
impl Help {
    pub(crate) fn handler(
        definitions: Vec<ToolDefinition>,
        conversations: QqConversations,
    ) -> Arc<dyn QqCommandHandler> {
        Arc::new(Self {
            definitions,
            conversations,
        })
    }
}
impl QqCommandHandler for Help {
    fn handle(&self, input: QqCommandInput<'_>) -> PluginResult<Option<String>> {
        let words: Vec<_> = input.text.split_whitespace().collect();
        if words.first() != Some(&"/help") {
            return Ok(None);
        }
        if words.as_slice() == ["/help", "identity"] {
            let account = self
                .conversations
                .identity(input.session)?
                .map(|identity| identity.account_id())
                .unwrap_or_else(|| "平台身份尚未就绪".into());
            return Ok(Some(format!(
                "平台账号标识：{account}\n本会话宿主绑定的操作者标识：{}\n主人请在受信面板 runtime.people.owner_accounts 绑定平台账号标识，同一群聊账号跨群有效。旧 owner_principals 与 operators 仍按会话标识兼容。/admin 查看角色；/bind 密钥 绑定管理员。群聊与私聊属于不同平台命名空间，须分别核实绑定，不按昵称或相同原始字符串自动关联。",
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
            text.push_str("每轮最多 100 次工具调用；有工具结果时途中汇报 1–3 次，达到上限后依据已有结果收尾。\n");
            for definition in &self.definitions {
                let description: String = definition.description.chars().take(52).collect();
                text.push_str(&format!("\n{}：{description}", definition.name));
            }
        } else {
            text.push_str("\n/admin：查看宿主确认的管理员状态\n/model、/model 模型名：查看或全局切换主模型（切换仅主人/管理员）\n/bind 密钥：绑定管理员（私聊与群聊分别绑定）\n/name 名字、/name：设置或查看自己的称呼\n/train start|stop|status：主动提问训练\n/self-learning status：自主学习状态\n/segment on|off|reset：分段输出\n/remember 内容、/memories、/recall 关键词：记忆\n/interests：兴趣；/knowledge 兴趣ID：资料\n/practice 兴趣ID：实践证据\n/skills、/tools：已固化技能和检查规则，列表可为空\n/goals、/mind、/plans：目标与计划\n/budget：后台自主运行预算\n/clarify、/clarify off|on：卡住时的提问\n/outreach、/outreach off|on：主动跟进\n/last-reply：取回当前会话发送失败的回复，不重新执行任务\n/cancel：取消当前任务\n/help tools：实际聊天工具说明\n\n命令对应能力未装配时会明确提示。工具调用以真实结果为准；检查规则不等于任意代码插件。");
        }
        Ok(Some(text))
    }
}
