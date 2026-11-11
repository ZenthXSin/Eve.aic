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
}

export interface ModelRequest {
  messages: Message[];
  tools: ToolSpec[];
}

export interface ModelResponse {
  content: string;
  toolCalls: ToolCall[];
}
