// 模型契约：一次补全请求进，一次回复出。具体接哪家模型由实现决定。
import type { Json } from "../kernel/protocol.ts";

export const MODEL = "model";

export interface ToolCall {
  id: string;
  name: string;
  args: Json;
}

export const MODEL_IMAGE_MAX_BYTES = 8 * 1024 * 1024;
export const MODEL_IMAGE_MAX_COUNT = 8;

/** 已读取的原始附件；传输层直接编码为图像输入，不把 base64 作为文字交给模型。 */
export interface ModelImage {
  dataUrl: string;
  provenance: {
    kind: "phone_attachment";
    conversationId: string;
    sourceSeq: number;
    attachmentId: string;
    sha256: string;
    mimeType: "image/png" | "image/jpeg" | "image/webp" | "image/gif";
    senderPlatformUserId?: string;
  };
}

/** 默认由同一配置模型先独立描述附件，再把文字与来源交给主意识。 */
export interface ImageDescription {
  description: string;
  provenance: ModelImage["provenance"];
  model?: string;
}

export interface Message {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  toolCalls?: ToolCall[];
  toolCallId?: string;
  /** 仅 user 消息允许；工具结果中的图片须在全部工具配对后作为 user 图像输入。 */
  images?: ModelImage[];
}

export interface ToolSpec {
  name: string;
  description: string;
  /** 真实工具参数的 JSON Schema；联网模型发送函数定义时必须具备。 */
  parameters?: { [key: string]: Json };
}

/** 每次调用独立选择，复用连接池；不修改其他会话默认配置。 */
export interface ModelCallOptions {
  model?: string;
  reasoningEffort?: string;
  maxOutputTokens?: number;
  sessionId?: string;
}

export interface ModelRequest {
  messages: Message[];
  tools: ToolSpec[];
}

export interface ModelResponse {
  content: string;
  toolCalls: ToolCall[];
  /** 不存在的用量字段保持未知，尤其不能把缺失缓存统计写成 0。 */
  usage?: {
    status: "reported" | "partial" | "invalid";
    inputTokens?: number;
    outputTokens?: number;
    cachedInputTokens?: number;
    totalTokens?: number;
  };
}
