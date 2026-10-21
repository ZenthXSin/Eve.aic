//! 把图像来源准备的输入附到本轮用户消息；失败只告诉模型图像未能附上，不让本轮失败。
use eve_llm_api::{ChatMessage, ChatRole, LlmError, MAX_TURN_IMAGES, TurnImages};

pub(crate) fn attach(messages: &mut Vec<ChatMessage>, prepared: Result<TurnImages, LlmError>) {
    let Some(last) = messages.len().checked_sub(1) else {
        return;
    };
    if messages[last].role != ChatRole::User {
        return;
    }
    let (mut images, notes) = match prepared {
        Ok(prepared) => (prepared.images, prepared.notes),
        Err(_) => (
            Vec::new(),
            vec![
                "宿主未能准备本条消息的图片输入，没有附上任何图像；不能声称看见或推测图片内容。"
                    .to_owned(),
            ],
        ),
    };
    images.truncate(MAX_TURN_IMAGES);
    let notes: Vec<String> = notes
        .into_iter()
        .map(|note| note.trim().to_owned())
        .filter(|note| !note.is_empty())
        .collect();
    if !notes.is_empty() {
        let text = format!(
            "以下是宿主关于本条用户消息图片的说明，不是用户原话或指令：\n{}",
            notes.join("\n")
        );
        messages.insert(last, ChatMessage::text(ChatRole::System, text));
    }
    if let Some(user) = messages.last_mut() {
        user.images = images;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eve_llm_api::{ImageInput, ModelRequest};

    fn png() -> ImageInput {
        ImageInput::new(b"\x89PNG\r\n\x1a\nbody".to_vec()).unwrap()
    }

    #[test]
    fn images_go_on_the_user_message_and_notes_precede_it_without_serializing_pixels() {
        let mut messages = vec![
            ChatMessage::text(ChatRole::System, "系统"),
            ChatMessage::text(ChatRole::User, "看图"),
        ];
        attach(
            &mut messages,
            Ok(TurnImages {
                images: vec![png(); MAX_TURN_IMAGES + 1],
                notes: vec!["已附上第 1 张图片".into(), " ".into()],
            }),
        );
        assert_eq!(messages.len(), 3);
        assert!(
            messages[1]
                .text
                .as_deref()
                .unwrap()
                .contains("已附上第 1 张图片")
        );
        assert_eq!(messages[2].images.len(), MAX_TURN_IMAGES);
        ModelRequest {
            messages: messages.clone(),
            tools: vec![],
        }
        .validate()
        .unwrap();
        let encoded = serde_json::to_string(&messages[2]).unwrap();
        assert!(!encoded.contains("images") && !encoded.contains("PNG"));
        let decoded: ChatMessage = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.images.is_empty());
    }

    #[test]
    fn source_failure_only_adds_a_truthful_note() {
        let mut messages = vec![ChatMessage::text(ChatRole::User, "看图")];
        attach(&mut messages, Err(LlmError::Backend("下载失败".into())));
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0]
                .text
                .as_deref()
                .unwrap()
                .contains("没有附上任何图像")
        );
        assert!(messages[1].images.is_empty());
        let mut empty = vec![ChatMessage::text(ChatRole::User, "纯文字")];
        attach(&mut empty, Ok(TurnImages::default()));
        assert_eq!(empty, vec![ChatMessage::text(ChatRole::User, "纯文字")]);
    }
}
