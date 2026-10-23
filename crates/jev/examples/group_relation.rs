//! 合成问答的真实 Jev 协议验收；凭据只从宿主环境读取，不连接聊天平台。
use eve_chime_api::*;
use eve_jev::{JevConfig, JevRelationJudge};
fn message(id: &str, text: &str, me: bool) -> GroupMessage {
    GroupMessage {
        id: id.into(),
        conversation: "synthetic-group".into(),
        speaker: if me { "eve" } else { "member" }.into(),
        speaker_name: None,
        text: text.into(),
        mentions_me: false,
        replies_to_me: false,
        from_me: me,
        at_ms: 1,
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let judge = JevRelationJudge::new(
        JevConfig {
            base_url: std::env::var("EVE_JEV_BASE_URL")?,
            model: std::env::var("EVE_JEV_MODEL").unwrap_or_else(|_| eve_jev::DEFAULT_MODEL.into()),
            ..JevConfig::default()
        },
        &std::env::var("EVE_JEV_API_KEY")?,
    )?;
    let exchange = GroupExchange {
        source: message(
            "original",
            "Mindustry 的逻辑处理器怎么控制单位挖矿？",
            false,
        ),
        reply: message("answer", "先绑定单位，再用雷达寻找矿点并执行采矿指令", true),
    };
    for (name, text, history) in [
        (
            "mindustry_question",
            "Mindustry 的逻辑处理器怎么控制单位挖矿？",
            vec![],
        ),
        (
            "related_followup",
            "你刚才说的绑定单位，用哪条逻辑指令？",
            vec![exchange.clone()],
        ),
        (
            "unrelated",
            "小王，今晚八点来我家吃饭，咱们上次约好的",
            vec![exchange],
        ),
    ] {
        let result = judge
            .classify(GroupRelationInput {
                message: message(name, text, false),
                exchanges: history,
                examples: vec![],
            })
            .await?;
        println!("{}", serde_json::json!({"case":name,"decision":result}));
    }
    Ok(())
}
