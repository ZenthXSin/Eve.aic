import type { Json } from "../../kernel/protocol.ts";
import {
  CHAT_DESCRIPTION,
  CHAT_INPUT,
  CHAT_MODE,
  CHAT_PAGE,
  CHAT_REPLY_BYTES,
  chatError,
  type ChatMessage,
  type ChatRequest,
  chatRequest,
  plainText,
  publicChatError,
  record,
  UUID4,
} from "./studio-chat-contract.ts";

const MAX_BODY = 32_768;
const headers = {
  "cache-control": "no-store",
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
};
const failure = (status: number, message: string) =>
  Response.json({ error: { message } }, { status, headers });

function publicMessage(value: unknown): ChatMessage {
  if (
    !record(value) || !Number.isSafeInteger(value.seq) || Number(value.seq) < 1 ||
    typeof value.at !== "string" || value.at.length !== 24 ||
    !Number.isFinite(Date.parse(value.at)) ||
    new Date(value.at).toISOString() !== value.at || typeof value.id !== "string"
  ) throw chatError("unavailable");
  const { id, at } = value, seq = Number(value.seq);
  if (
    value.role === "user" &&
    ["queued", "processing", "answered", "failed", "unknown"].includes(String(value.status)) &&
    id.length === 36 && UUID4.test(id) && plainText(value.text, CHAT_INPUT, CHAT_INPUT * 3)
  ) {
    return {
      seq,
      at,
      id,
      role: "user",
      text: value.text,
      status: value.status as ChatMessage["status"],
    };
  }
  if (
    value.role === "assistant" && value.status === "sent" && typeof value.replyTo === "string" &&
    value.replyTo.length === 36 && UUID4.test(value.replyTo) && id === `reply:${value.replyTo}` &&
    plainText(value.text, CHAT_REPLY_BYTES, CHAT_REPLY_BYTES)
  ) {
    return {
      seq,
      at,
      id,
      role: "assistant",
      replyTo: value.replyTo,
      text: value.text,
      status: "sent",
    };
  }
  throw chatError("unavailable");
}

async function sendBody(request: Request): Promise<ChatRequest | Response> {
  if (
    !/^application\/json(?:\s*;\s*charset=utf-8)?$/i.test(
      request.headers.get("content-type") ?? "",
    ) ||
    request.headers.has("content-encoding")
  ) return failure(415, "仅接受 UTF-8 JSON");
  const length = request.headers.get("content-length");
  if (length !== null && (!/^\d+$/.test(length) || Number(length) > MAX_BODY)) {
    return failure(413, "请求过大");
  }
  if (!request.body) throw chatError("invalid");
  const reader = request.body.getReader(), buffer = new Uint8Array(MAX_BODY);
  let size = 0, expired = false;
  const timer = setTimeout(() => {
    expired = true;
    void reader.cancel().catch(() => {});
  }, 5000);
  try {
    while (true) {
      const next = await reader.read();
      if (expired) return failure(408, "读取聊天请求超时");
      if (next.done) break;
      if (next.value.length > MAX_BODY - size) {
        void reader.cancel().catch(() => {});
        return failure(413, "请求过大");
      }
      buffer.set(next.value, size);
      size += next.value.length;
    }
    const body: unknown = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(buffer.subarray(0, size)),
    );
    if (!record(body) || Object.keys(body).length !== 2 || !("id" in body) || !("text" in body)) {
      throw chatError("invalid");
    }
    return chatRequest({ action: "chatSend", id: body.id, text: body.text });
  } catch {
    throw chatError("invalid");
  } finally {
    clearTimeout(timer);
    reader.releaseLock();
  }
}

export function studioChatRoutes(invoke: (request: ChatRequest) => Promise<Json>) {
  let active = 0;
  return async (
    request: Request,
    session: { csrf: string } | undefined,
    origin: string,
  ): Promise<Response | undefined> => {
    const url = new URL(request.url), read = url.pathname === "/api/chat";
    if (!read && url.pathname !== "/api/chat/send") return;
    if (!session?.csrf) return failure(401, "请先登录");
    if (request.headers.has("origin") && request.headers.get("origin") !== origin) {
      return failure(403, "需要同源请求");
    }
    if (request.method !== (read ? "GET" : "POST")) return failure(405, "聊天请求方法无效");
    if (!read && request.headers.get("x-eve-csrf") !== session.csrf) {
      return failure(403, "聊天请求校验失败");
    }
    if (active >= 8) return failure(429, "聊天请求过多");
    active++;
    try {
      if (!read) {
        if (url.search) throw chatError("invalid");
        const send = await sendBody(request);
        if (send instanceof Response) return send;
        if (send.action !== "chatSend") throw chatError("invalid");
        const value = await invoke(send);
        if (!record(value)) throw chatError("unavailable");
        const message = publicMessage(value.message);
        if (message.role !== "user" || message.id !== send.id || message.text !== send.text) {
          throw chatError("unavailable");
        }
        return Response.json({ message, mode: CHAT_MODE }, {
          status: ["queued", "processing"].includes(message.status) ? 202 : 200,
          headers,
        });
      }
      const params = [...url.searchParams];
      if (
        params.length &&
        (params.length !== 1 || params[0][0] !== "before" || !/^[1-9]\d{0,15}$/.test(params[0][1]))
      ) {
        throw chatError("invalid");
      }
      const before = params.length ? Number(params[0][1]) : undefined;
      const value = await invoke(
        chatRequest(before === undefined ? { action: "chat" } : { action: "chat", before }),
      );
      if (!record(value) || !Array.isArray(value.messages) || value.messages.length > CHAT_PAGE) {
        throw chatError("unavailable");
      }
      const messages = value.messages.map(publicMessage), nextBefore = value.nextBefore;
      if (
        messages.some((msg, index) =>
          (index > 0 && messages[index - 1].seq >= msg.seq) ||
          (before !== undefined && msg.seq >= before)
        ) ||
        (nextBefore !== null && (!messages.length || nextBefore !== messages[0].seq))
      ) throw chatError("unavailable");
      return Response.json(
        { messages, nextBefore, mode: CHAT_MODE, description: CHAT_DESCRIPTION },
        { headers },
      );
    } catch (error) {
      const [status, message] = publicChatError(error);
      return failure(status, message);
    } finally {
      active--;
    }
  };
}
