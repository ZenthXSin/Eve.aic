import { ChatModelError } from "./errors.ts";
export { ChatModelError } from "./errors.ts";
// Chat Completions 传输。配置与凭据捕获一次，单一稳定会话，不设置 temperature。
import {
  MODEL_IMAGE_MAX_BYTES,
  MODEL_IMAGE_MAX_COUNT,
  type ModelCallOptions,
  type ModelImage,
  type ModelRequest,
  type ModelResponse,
  type ToolCall,
} from "../../contracts/model.ts";
import { MODEL_MAX_DURATION_MS } from "../../sdk/model-timeouts.ts";
import { ChatStream } from "./stream.ts";
import { permanentModelStatus } from "../../sdk/model-failure.ts";

export interface ChatModelOptions {
  baseUrl: string;
  model: string;
  apiKey: string;
  sessionId: string;
  reasoningEffort?: string;
  maxOutputTokens: number;
  /** Legacy setting now supplies the first-token and progress-idle deadlines. */
  timeoutMs: number;
  connectTimeoutMs?: number;
  firstTokenTimeoutMs?: number;
  idleTimeoutMs?: number;
  maxDurationMs?: number;
  maxResponseBytes?: number;
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

function imageBytes(value: unknown): { image: ModelImage; bytes: Uint8Array<ArrayBuffer> } {
  if (!record(value) || typeof value.dataUrl !== "string" || !record(value.provenance)) {
    fail("request");
  }
  const p = value.provenance;
  if (
    p.kind !== "phone_attachment" || typeof p.conversationId !== "string" ||
    !p.conversationId || p.conversationId.length > 512 ||
    !Number.isSafeInteger(p.sourceSeq) || (p.sourceSeq as number) < 1 ||
    typeof p.attachmentId !== "string" || !p.attachmentId || p.attachmentId.length > 1024 ||
    typeof p.sha256 !== "string" || !/^[a-f0-9]{64}$/.test(p.sha256) ||
    (p.senderPlatformUserId !== undefined &&
      (typeof p.senderPlatformUserId !== "string" || !p.senderPlatformUserId ||
        p.senderPlatformUserId.length > 512))
  ) fail("request");
  // 仅接受内嵌附件；禁止上游代取任意 URL，并在解码前限制长度。
  const maxEncoded = Math.ceil(MODEL_IMAGE_MAX_BYTES / 3) * 4;
  if (value.dataUrl.length > maxEncoded + 64) fail("request");
  const match = /^data:(image\/(?:png|jpeg|webp|gif));base64,([A-Za-z0-9+/]*={0,2})$/.exec(
    value.dataUrl,
  );
  if (!match || match[1] !== p.mimeType || !match[2] || match[2].length % 4 !== 0) {
    fail("request");
  }
  let raw: string;
  try {
    raw = atob(match[2]);
  } catch {
    fail("request");
  }
  if (raw.length > MODEL_IMAGE_MAX_BYTES || btoa(raw) !== match[2]) fail("request");
  const bytes = Uint8Array.from(raw, (character) => character.charCodeAt(0));
  const starts = (...prefix: number[]) => prefix.every((byte, index) => bytes[index] === byte);
  const signature = match[1] === "image/png"
    ? starts(137, 80, 78, 71, 13, 10, 26, 10)
    : match[1] === "image/jpeg"
    ? starts(255, 216, 255)
    : match[1] === "image/gif"
    ? raw.startsWith("GIF87a") || raw.startsWith("GIF89a")
    : raw.startsWith("RIFF") && raw.slice(8, 12) === "WEBP";
  if (!signature) fail("request");
  return { image: value as unknown as ModelImage, bytes };
}

async function requestBody(
  request: ModelRequest,
  config: ChatModelOptions,
): Promise<Record<string, unknown>> {
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
  let imageCount = 0;
  let imageSize = 0;
  const imageChecks: { sha256: string; bytes: Uint8Array<ArrayBuffer> }[] = [];
  const messages = request.messages.map((message) => {
    if (
      !record(message) || !["system", "user", "assistant", "tool"].includes(message.role) ||
      typeof message.content !== "string"
    ) fail("request");
    let imageContent: Record<string, unknown>[] | undefined;
    if (message.images !== undefined) {
      if (message.role !== "user" || !Array.isArray(message.images)) fail("request");
      imageCount += message.images.length;
      if (imageCount > MODEL_IMAGE_MAX_COUNT) fail("request");
      imageContent = message.images.map((value) => {
        const { image, bytes } = imageBytes(value);
        imageSize += bytes.length;
        if (imageSize > MODEL_IMAGE_MAX_BYTES) fail("request");
        imageChecks.push({ sha256: image.provenance.sha256, bytes });
        return { type: "image_url", image_url: { url: image.dataUrl, detail: "auto" } };
      });
    }
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
      content: imageContent?.length
        ? [{ type: "text", text: message.content }, ...imageContent]
        : wireCalls.length > 0 && message.content === ""
        ? null
        : message.content,
      ...(wireCalls.length > 0 ? { tool_calls: wireCalls } : {}),
    };
  });
  if (messages.length === 0 || pending.size > 0) fail("request");
  await Promise.all(imageChecks.map(async ({ sha256, bytes }) => {
    const hash = await crypto.subtle.digest("SHA-256", bytes);
    const digest = Array.from(new Uint8Array(hash), (n) => n.toString(16).padStart(2, "0")).join(
      "",
    );
    if (digest !== sha256) fail("request");
  }));
  return {
    model: config.model,
    messages,
    ...(tools.length > 0 ? { tools } : {}),
    ...(config.reasoningEffort ? { reasoning_effort: config.reasoningEffort } : {}),
    max_completion_tokens: config.maxOutputTokens,
    prompt_cache_key: config.sessionId,
    stream: true,
    stream_options: { include_usage: true },
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
    const config = {
      ...options,
      connectTimeoutMs: options.connectTimeoutMs ?? options.timeoutMs,
      firstTokenTimeoutMs: options.firstTokenTimeoutMs ?? options.timeoutMs,
      idleTimeoutMs: options.idleTimeoutMs ?? options.timeoutMs,
      maxDurationMs: options.maxDurationMs ?? MODEL_MAX_DURATION_MS,
      maxResponseBytes: options.maxResponseBytes ?? 2_097_152,
    };
    if (
      typeof config.model !== "string" || !config.model.trim() ||
      typeof config.apiKey !== "string" || !config.apiKey.trim() ||
      typeof config.sessionId !== "string" || !/^[a-zA-Z0-9_.:-]{1,256}$/.test(config.sessionId) ||
      !positive(config.maxOutputTokens) || !positive(config.timeoutMs) ||
      config.timeoutMs > 2_147_483_647 || !positive(config.maxResponseBytes) ||
      ![
        config.connectTimeoutMs,
        config.firstTokenTimeoutMs,
        config.idleTimeoutMs,
        config.maxDurationMs,
      ].every((n) => positive(n) && n <= MODEL_MAX_DURATION_MS) ||
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

  async complete(request: ModelRequest, overrides: ModelCallOptions = {}): Promise<ModelResponse> {
    const config = { ...this.#config, ...overrides };
    if (
      !positive(config.maxOutputTokens) || typeof config.model !== "string" ||
      !config.model.trim() ||
      typeof config.sessionId !== "string" || !/^[a-zA-Z0-9_.:-]{1,256}$/.test(config.sessionId) ||
      overrides.reasoningEffort !== undefined &&
        !["none", "minimal", "low", "medium", "high", "xhigh"].includes(overrides.reasoningEffort)
    ) fail("request");
    let snapshot: ModelRequest;
    let body: string;
    try {
      snapshot = structuredClone(request);
      body = JSON.stringify(await requestBody(snapshot, config));
    } catch (error) {
      if (error instanceof ChatModelError) throw error;
      fail("request");
    }
    const controller = new AbortController();
    let timeoutPhase: ChatModelError["timeoutPhase"];
    let timer: ReturnType<typeof setTimeout> | undefined;
    const expire = (phase: NonNullable<ChatModelError["timeoutPhase"]>) => {
      if (!controller.signal.aborted) {
        timeoutPhase = phase;
        controller.abort();
      }
    };
    const arm = (phase: "connect" | "first_token" | "idle", ms: number) => {
      clearTimeout(timer);
      timer = setTimeout(() => expire(phase), ms);
    };
    const totalTimer = setTimeout(() => expire("total"), config.maxDurationMs!);
    const signal = AbortSignal.any([this.#closed.signal, controller.signal]);
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      arm("connect", config.connectTimeoutMs!);
      const response = await fetch(this.#endpoint, {
        method: "POST",
        redirect: "manual",
        signal,
        headers: {
          Authorization: `Bearer ${this.#config.apiKey}`,
          "Content-Type": "application/json",
          Accept: "text/event-stream",
          session_id: config.sessionId,
          "X-Session-ID": config.sessionId,
        },
        body,
      });
      if (response.status !== 200) {
        await response.body?.cancel();
        throw new ChatModelError(
          permanentModelStatus(response.status) ? "provider_rejected" : "http",
          response.status,
        );
      }
      if (!response.body) fail("invalid_response");
      arm("first_token", config.firstTokenTimeoutMs!);
      reader = response.body.getReader();
      const limit = config.maxResponseBytes!;
      const isStream = response.headers.get("content-type")?.split(";", 1)[0].trim()
        .toLowerCase() === "text/event-stream";
      const decoder = new TextDecoder("utf-8", { fatal: true });
      const decode = (bytes?: Uint8Array, streaming = false) => {
        try {
          return decoder.decode(bytes, { stream: streaming });
        } catch {
          fail("invalid_response");
        }
      };
      const stream = isStream
        ? new ChatStream(limit, () => arm("idle", config.idleTimeoutMs!))
        : null;
      let bytes = 0;
      const chunks: string[] = [];
      while (true) {
        const next = await reader.read();
        if (next.done) break;
        bytes += next.value.length;
        // SSE repeats field names per token. Bound wire overhead separately from retained output.
        if (bytes > (isStream ? limit * 64 + 1_048_576 : limit)) fail("invalid_response");
        const text = decode(next.value, true);
        if (stream) {
          stream.feed(text);
          if (stream.done) break;
        } else {
          // Compatibility for upstreams that ignore stream and return a complete JSON document.
          chunks.push(text);
        }
      }
      const tail = decode();
      if (stream) {
        stream.feed(tail);
        return modelResponse(stream.result(), snapshot);
      }
      chunks.push(tail);
      let decoded: unknown;
      try {
        decoded = JSON.parse(chunks.join(""));
      } catch {
        fail("invalid_response");
      }
      return modelResponse(decoded, snapshot);
    } catch (error) {
      if (timeoutPhase) throw new ChatModelError("transport", undefined, timeoutPhase);
      if (error instanceof ChatModelError) throw error;
      throw new ChatModelError("transport");
    } finally {
      clearTimeout(timer);
      clearTimeout(totalTimer);
      controller.abort();
      if (reader) {
        try {
          await reader.cancel();
        } catch { /* The fetch may already have been aborted. */ }
        reader.releaseLock();
      }
    }
  }
}
