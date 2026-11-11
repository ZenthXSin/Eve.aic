// Chat Completions 传输。配置与凭据捕获一次，单一稳定会话，不设置 temperature。
import type { ModelRequest, ModelResponse, ToolCall } from "../../contracts/model.ts";

export interface ChatModelOptions {
  baseUrl: string;
  model: string;
  apiKey: string;
  sessionId: string;
  reasoningEffort?: string;
  maxOutputTokens: number;
  timeoutMs: number;
  maxResponseBytes?: number;
}

export class ChatModelError extends Error {
  constructor(
    readonly category: "configuration" | "request" | "transport" | "http" | "invalid_response",
    readonly httpStatus?: number,
  ) {
    // 服务端正文、URL 和 fetch 原文可能含凭据，不写入错误消息或 cause。
    super(`模型请求未完成：${category}${httpStatus === undefined ? "" : ` (${httpStatus})`}`);
    this.name = "ChatModelError";
  }
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function positive(value: number): boolean {
  return Number.isSafeInteger(value) && value > 0;
}

function fail(category: ChatModelError["category"]): never {
  throw new ChatModelError(category);
}

function requestBody(request: ModelRequest, config: ChatModelOptions): Record<string, unknown> {
  if (!record(request) || !Array.isArray(request.messages) || !Array.isArray(request.tools)) {
    fail("request");
  }
  const names = new Set<string>();
  const tools = request.tools.map((tool) => {
    if (
      !record(tool) || typeof tool.name !== "string" || !/^[a-zA-Z0-9_-]{1,64}$/.test(tool.name) ||
      names.has(tool.name) || typeof tool.description !== "string" || !record(tool.parameters) ||
      tool.parameters.type !== "object"
    ) fail("request");
    names.add(tool.name);
    return {
      type: "function",
      function: { name: tool.name, description: tool.description, parameters: tool.parameters },
    };
  });
  const pending = new Set<string>();
  const seenIds = new Set<string>();
  const messages = request.messages.map((message) => {
    if (
      !record(message) || !["system", "user", "assistant", "tool"].includes(message.role) ||
      typeof message.content !== "string"
    ) fail("request");
    if (message.role === "tool") {
      if (typeof message.toolCallId !== "string" || !pending.delete(message.toolCallId)) {
        fail("request");
      }
      return { role: "tool", content: message.content, tool_call_id: message.toolCallId };
    }
    if (pending.size > 0) fail("request");
    const calls = message.toolCalls ?? [];
    if (!Array.isArray(calls) || (calls.length > 0 && message.role !== "assistant")) {
      fail("request");
    }
    const wireCalls = calls.map((call) => {
      if (
        !record(call) || typeof call.id !== "string" || !call.id || seenIds.has(call.id) ||
        typeof call.name !== "string" || !record(call.args)
      ) fail("request");
      seenIds.add(call.id);
      pending.add(call.id);
      return {
        id: call.id,
        type: "function",
        function: { name: call.name, arguments: JSON.stringify(call.args) },
      };
    });
    return {
      role: message.role,
      content: wireCalls.length > 0 && message.content === "" ? null : message.content,
      ...(wireCalls.length > 0 ? { tool_calls: wireCalls } : {}),
    };
  });
  if (messages.length === 0 || pending.size > 0) fail("request");
  return {
    model: config.model,
    messages,
    ...(tools.length > 0 ? { tools } : {}),
    ...(config.reasoningEffort ? { reasoning_effort: config.reasoningEffort } : {}),
    max_completion_tokens: config.maxOutputTokens,
    prompt_cache_key: config.sessionId,
    stream: false,
  };
}

function usageView(raw: unknown): ModelResponse["usage"] {
  if (raw === undefined) return undefined;
  if (!record(raw)) return { status: "invalid" };
  const usage: NonNullable<ModelResponse["usage"]> = { status: "partial" };
  let invalid = false;
  for (
    const [wire, field] of [
      ["prompt_tokens", "inputTokens"],
      ["completion_tokens", "outputTokens"],
      ["total_tokens", "totalTokens"],
    ] as const
  ) {
    if (raw[wire] === undefined) continue;
    const value = raw[wire];
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) invalid = true;
    else usage[field] = value;
  }
  if (raw.prompt_tokens_details !== undefined) {
    if (!record(raw.prompt_tokens_details)) invalid = true;
    else if (raw.prompt_tokens_details.cached_tokens !== undefined) {
      const cached = raw.prompt_tokens_details.cached_tokens;
      if (
        typeof cached !== "number" || !Number.isSafeInteger(cached) || cached < 0 ||
        (usage.inputTokens !== undefined && cached > usage.inputTokens)
      ) invalid = true;
      else usage.cachedInputTokens = cached;
    }
  }
  usage.status = invalid
    ? "invalid"
    : usage.inputTokens !== undefined && usage.outputTokens !== undefined &&
        usage.cachedInputTokens !== undefined
    ? "reported"
    : "partial";
  return usage;
}

function modelResponse(raw: unknown, request: ModelRequest): ModelResponse {
  if (!record(raw) || !Array.isArray(raw.choices) || raw.choices.length !== 1) {
    fail("invalid_response");
  }
  const choice = raw.choices[0];
  if (
    !record(choice) || !["stop", "tool_calls"].includes(String(choice.finish_reason)) ||
    !record(choice.message) || choice.message.role !== "assistant"
  ) fail("invalid_response");
  const message = choice.message;
  if (
    message.content !== null && message.content !== undefined && typeof message.content !== "string"
  ) fail("invalid_response");
  // 有些 Chat 兼容上游在纯工具响应中省略 content；仍须完整验证工具，空终答继续拒绝。
  const content = (message.content as string | null | undefined) ??
    (typeof message.refusal === "string" ? message.refusal : "");
  const rawCalls = message.tool_calls ?? [];
  if (!Array.isArray(rawCalls)) fail("invalid_response");
  const tools = new Set(request.tools.map((tool) => tool.name));
  const ids = new Set<string>();
  const toolCalls: ToolCall[] = rawCalls.map((call) => {
    if (
      !record(call) || call.type !== "function" || typeof call.id !== "string" || !call.id ||
      ids.has(call.id) || !record(call.function) || typeof call.function.name !== "string" ||
      !tools.has(call.function.name) || typeof call.function.arguments !== "string"
    ) fail("invalid_response");
    let args: unknown;
    try {
      args = JSON.parse(call.function.arguments);
    } catch {
      fail("invalid_response");
    }
    if (!record(args)) fail("invalid_response");
    ids.add(call.id);
    return { id: call.id, name: call.function.name, args: args as ToolCall["args"] };
  });
  if (
    (toolCalls.length === 0 && content.trim() === "") ||
    (choice.finish_reason === "tool_calls" && toolCalls.length === 0)
  ) fail("invalid_response");
  const usage = usageView(raw.usage);
  return { content, toolCalls, ...(usage === undefined ? {} : { usage }) };
}

export class ChatModelClient {
  #config: ChatModelOptions;
  #endpoint: URL;
  #closed = new AbortController();

  constructor(options: ChatModelOptions) {
    const config = { ...options, maxResponseBytes: options.maxResponseBytes ?? 2_097_152 };
    if (
      typeof config.model !== "string" || !config.model.trim() ||
      typeof config.apiKey !== "string" || !config.apiKey.trim() ||
      typeof config.sessionId !== "string" || !/^[a-zA-Z0-9_.:-]{1,256}$/.test(config.sessionId) ||
      !positive(config.maxOutputTokens) || !positive(config.timeoutMs) ||
      config.timeoutMs > 2_147_483_647 || !positive(config.maxResponseBytes) ||
      (config.reasoningEffort !== undefined &&
        (typeof config.reasoningEffort !== "string" || !config.reasoningEffort.trim()))
    ) fail("configuration");
    let base: URL;
    try {
      base = new URL(config.baseUrl);
    } catch {
      fail("configuration");
    }
    if (
      base.username || base.password || base.search || base.hash ||
      (base.protocol !== "https:" &&
        !(base.protocol === "http:" && ["127.0.0.1", "localhost", "[::1]"].includes(base.hostname)))
    ) fail("configuration");
    const path = base.pathname.replace(/\/$/, "");
    base.pathname = path.endsWith("/v1")
      ? `${path}/chat/completions`
      : `${path}/v1/chat/completions`;
    this.#endpoint = base;
    this.#config = config;
  }

  close(): void {
    this.#closed.abort();
  }

  async complete(request: ModelRequest): Promise<ModelResponse> {
    let snapshot: ModelRequest;
    let body: string;
    try {
      snapshot = structuredClone(request);
      body = JSON.stringify(requestBody(snapshot, this.#config));
    } catch (error) {
      if (error instanceof ChatModelError) throw error;
      fail("request");
    }
    try {
      const response = await fetch(this.#endpoint, {
        method: "POST",
        redirect: "manual",
        signal: AbortSignal.any([
          this.#closed.signal,
          AbortSignal.timeout(this.#config.timeoutMs),
        ]),
        headers: {
          Authorization: `Bearer ${this.#config.apiKey}`,
          "Content-Type": "application/json",
          session_id: this.#config.sessionId,
          "X-Session-ID": this.#config.sessionId,
        },
        body,
      });
      if (response.status !== 200) {
        await response.body?.cancel();
        throw new ChatModelError("http", response.status);
      }
      if (!response.body) fail("invalid_response");
      const reader = response.body.getReader();
      const chunks: Uint8Array[] = [];
      let bytes = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          bytes += next.value.length;
          if (bytes > this.#config.maxResponseBytes!) {
            await reader.cancel();
            fail("invalid_response");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const data = new Uint8Array(bytes);
      let offset = 0;
      for (const chunk of chunks) {
        data.set(chunk, offset);
        offset += chunk.length;
      }
      let decoded: unknown;
      try {
        decoded = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(data));
      } catch {
        fail("invalid_response");
      }
      return modelResponse(decoded, snapshot);
    } catch (error) {
      if (error instanceof ChatModelError) throw error;
      throw new ChatModelError("transport");
    }
  }
}
