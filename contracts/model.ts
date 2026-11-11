// 模型契约：一次补全请求进，一次回复出。具体接哪家模型由实现决定。
import type { Json } from "../kernel/protocol.ts";

export const MODEL = "model";

export interface ToolCall {
  id: string;
  name: string;
  args: Json;
}

export interface Message {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  toolCalls?: ToolCall[];
  toolCallId?: string;
}

export interface ToolSpec {
  name: string;
  description: string;
  /** 真实工具参数的 JSON Schema；联网模型发送函数定义时必须具备。 */
  parameters?: { [key: string]: Json };
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
