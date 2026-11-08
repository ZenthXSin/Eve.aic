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
                "直接说你想做什么即可；/help 查看当前能力，/help tools 查看实际工具说明，/help identity 查看本会话操作者标识。".into(),
            ));
        }
        let names = self
            .definitions
            .iter()
            .map(|d| d.name.as_str())
            .collect::<Vec<_>>()
            .join("、");
        let mut text = format!(
            "直接聊天即可使用当前已装配的能力，例如请我安排提醒、回忆偏好、处理后台工作或查看真实进度。\n当前实际对话工具（{} 个）：{names}\n",
            self.definitions.len()
        );
        if words.len() == 2 {
            text.push_str("每轮最多 100 次工具调用；有工具结果时途中汇报 1–3 次，达到上限后依据已有结果收尾。\n");
            for definition in &self.definitions {
                let description: String = definition.description.chars().take(52).collect();
                text.push_str(&format!("\n{}：{description}", definition.name));
            }
        } else {
            text.push_str("\n可以直接问“你现在能做什么”“我的任务进行到哪了”“有哪些已经验证的技能”。需要运行环境的能力会说明实际缺少的条件；主人或管理员可以让我补齐并验证。技能列表为空表示尚未学会，工具执行以真实回执为准。\n\n管理权限依据宿主核验的身份；直接提出切换模型、解锁业务权限或停止任务等请求仍经过对应检查。兼容入口：/admin 查看角色，/bind 密钥绑定管理员，/help identity 查看可信账号标识，/last-reply 取回发送失败的回复，/cancel 取消当前任务，/help tools 查看工具说明，/effort 查看或调整思考强度。");
        }
        Ok(Some(text))
    }
}
