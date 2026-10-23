"""模型请求中的临时宿主提示不属于持久化用户轮次；实际请求保持原样。"""
HOST_STYLE_PREFIX = "宿主表达方式：下一条 User 才是真实消息，不回应本段。"

def conversation_messages(messages):
    return [message for message in messages if not (
        message.get("role") == "user" and isinstance(message.get("content"), str)
        and message["content"].startswith(HOST_STYLE_PREFIX))]
