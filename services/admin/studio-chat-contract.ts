import { KernelError } from "../../kernel/protocol.ts";

export const CHAT_MODE = "isolated";
export const CHAT_DESCRIPTION =
  "这是 Eve 的私有 Web 会话。Eve 在当前动作告一段落后，用当前模型和共享预算回复。只读取本会话的近期对话，不共享主意识记忆。Web 登录不代表 QQ 身份或操作授权；不会调用工具或向 QQ 发送消息。";
export const CHAT_PAGE = 20;
export const CHAT_TURNS = 512;
export const CHAT_INPUT = 4096;
export const CHAT_REPLY_BYTES = 8192;
export const UUID4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
export const bytes = (text: string) => new TextEncoder().encode(text).length;
export const record = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value);

export type ChatRequest = { action: "chat"; before?: number } | {
  action: "chatSend";
  id: string;
  text: string;
};
export type ChatStatus = "queued" | "processing" | "answered" | "failed" | "unknown";
export type ChatMessage = {
  seq: number;
  id: string;
  role: "user" | "assistant";
  text: string;
  status: ChatStatus | "sent";
  at: string;
  replyTo?: string;
};
export type ChatPage = {
  messages: ChatMessage[];
  nextBefore: number | null;
  mode: typeof CHAT_MODE;
  description: string;
};

const errors = {
  invalid: [400, "聊天参数无效"],
  conflict: [409, "同一消息编号不能更改正文"],
  busy: [429, "上一条私聊尚未处理，请先读取会话"],
  full: [507, "私聊存储已满；保留历史与幂等记录，不接受新消息"],
  unavailable: [503, "私聊暂不可用，请用原编号核对"],
} as const;

export function chatError(code: keyof typeof errors) {
  return new KernelError(code === "unavailable" ? "unavailable" : "rejected", errors[code][1]);
}

export function publicChatError(error: unknown) {
  const known = error instanceof KernelError &&
    Object.values(errors).find((entry) => entry[1] === error.message);
  return known || errors.unavailable;
}

export function plainText(value: unknown, maxUnits: number, maxBytes: number): value is string {
  return typeof value === "string" && value.length <= maxUnits && value.trim().length > 0 &&
    value.isWellFormed() && !/[\p{Cc}]/u.test(value.replace(/[\t\r\n]/g, "")) &&
    bytes(value) <= maxBytes;
}

export function chatRequest(value: unknown): ChatRequest {
  if (record(value)) {
    const keys = Object.keys(value);
    if (value.action === "chat" && keys.every((key) => key === "action" || key === "before")) {
      if (!("before" in value)) return { action: "chat" };
      if (
        typeof value.before === "number" && Number.isSafeInteger(value.before) && value.before > 0
      ) {
        return { action: "chat", before: value.before };
      }
    }
    if (
      value.action === "chatSend" && keys.length === 3 && typeof value.id === "string" &&
      value.id.length === 36 && UUID4.test(value.id) &&
      plainText(value.text, CHAT_INPUT, CHAT_INPUT * 3)
    ) return { action: "chatSend", id: value.id, text: value.text.trim() };
  }
  throw chatError("invalid");
}
