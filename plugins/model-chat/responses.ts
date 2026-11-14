import type { ModelRequest } from "../../contracts/model.ts";
import { ChatModelError } from "./errors.ts";

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function invalid(): never {
  throw new ChatModelError("invalid_response");
}
function requestInvalid(): never {
  throw new ChatModelError("request");
}

/** Input was already validated by the Chat request builder. No server-side history dependency. */
export function responsesBody(chat: Record<string, unknown>, request: ModelRequest) {
  const input: Record<string, unknown>[] = [];
  const instructions: string[] = [];
  const callIds = new Map<string, string>();
  const messages = chat.messages as Record<string, unknown>[];
  for (let i = 0; i < messages.length; i++) {
    const message = messages[i], source = request.messages[i];
    if (message.role === "system") {
      instructions.push(source.content);
      continue;
    }
    if (message.role === "tool") {
      input.push({
        type: "function_call_output",
        call_id: callIds.get(source.toolCallId!) ?? source.toolCallId,
        output: message.content,
      });
      continue;
    }
    if (message.role === "assistant" && source.modelOutput !== undefined) {
      const replay = source.modelOutput;
      if (!Array.isArray(replay)) requestInvalid();
      const nativeCalls = replay.filter((item) => object(item) && item.type === "function_call");
      const calls = source.toolCalls ?? [];
      if (nativeCalls.length !== calls.length) requestInvalid();
      const nativeIds = new Set<string>();
      for (let c = 0; c < calls.length; c++) {
        const native = nativeCalls[c] as Record<string, unknown>;
        if (
          native.name !== calls[c].name || typeof native.call_id !== "string" ||
          !native.call_id || nativeIds.has(native.call_id) || typeof native.arguments !== "string"
        ) requestInvalid();
        nativeIds.add(native.call_id);
        callIds.set(calls[c].id, native.call_id);
      }
      for (const item of replay) {
        if (
          !object(item) || !["message", "function_call", "reasoning"].includes(String(item.type))
        ) {
          requestInvalid();
        }
        if (item.type === "message" && item.role !== "assistant") requestInvalid();
        input.push(item);
      }
      continue;
    }
    if (message.content !== null && message.content !== "") {
      const role = message.role === "system" ? "developer" : message.role;
      const content = Array.isArray(message.content)
        ? message.content.map((part: Record<string, unknown>) =>
          part.type === "image_url"
            ? {
              type: "input_image",
              image_url: (part.image_url as { url: string }).url,
              detail: "auto",
            }
            : { type: "input_text", text: part.text }
        )
        : [{ type: role === "assistant" ? "output_text" : "input_text", text: message.content }];
      input.push({ role, content });
    }
    for (const call of (message.tool_calls ?? []) as Record<string, unknown>[]) {
      const fn = call.function as Record<string, unknown>;
      input.push({
        type: "function_call",
        call_id: call.id,
        name: fn.name,
        arguments: fn.arguments,
      });
    }
  }
  return {
    model: chat.model,
    ...(instructions.length ? { instructions: instructions.join("\n\n") } : {}),
    input,
    tools: ((chat.tools ?? []) as { function: Record<string, unknown> }[]).map((tool) => ({
      type: "function",
      ...tool.function,
      strict: false,
    })),
    ...(chat.reasoning_effort ? { reasoning: { effort: chat.reasoning_effort } } : {}),
    max_output_tokens: chat.max_completion_tokens,
    prompt_cache_key: chat.prompt_cache_key,
    store: false,
    include: ["reasoning.encrypted_content"],
    stream: true,
  };
}

/** Reuse the existing strict tool/name/argument/usage validation after protocol conversion. */
export function responsesResult(raw: unknown) {
  if (!object(raw) || raw.status !== "completed" || !Array.isArray(raw.output)) invalid();
  const text: string[] = [], calls: Record<string, unknown>[] = [];
  for (const item of raw.output) {
    if (!object(item)) invalid();
    if (item.type === "function_call") {
      calls.push({
        id: item.call_id,
        type: "function",
        function: { name: item.name, arguments: item.arguments },
      });
    } else if (item.type === "message") {
      if (item.role !== "assistant" || !Array.isArray(item.content)) invalid();
      for (const part of item.content) {
        if (!object(part)) invalid();
        if (part.type === "output_text" && typeof part.text === "string") text.push(part.text);
        else if (part.type === "refusal" && typeof part.refusal === "string") {
          text.push(part.refusal);
        } else invalid();
      }
    } else if (item.type !== "reasoning") invalid();
  }
  const usage = raw.usage;
  return {
    choices: [{
      finish_reason: calls.length ? "tool_calls" : "stop",
      message: { role: "assistant", content: text.join(""), tool_calls: calls },
    }],
    ...(usage === undefined ? {} : {
      usage: object(usage)
        ? {
          prompt_tokens: usage.input_tokens,
          completion_tokens: usage.output_tokens,
          total_tokens: usage.total_tokens,
          ...(usage.input_tokens_details === undefined
            ? {}
            : { prompt_tokens_details: usage.input_tokens_details }),
        }
        : usage,
    }),
  };
}

/** Keep only the final, complete output; partial argument deltas can never execute. */
export class ResponsesStream {
  done = false;
  #line = "";
  #cr = false;
  #data: string[] = [];
  #size = 0;
  #result: unknown;
  constructor(readonly limit: number, readonly progress: () => void) {}
  feed(text: string) {
    for (const char of text) {
      if (this.done) continue; // Some compatible servers append [DONE] after response.completed.
      if (this.#cr) {
        this.#cr = false;
        if (char === "\n") continue;
      }
      if (char === "\r" || char === "\n") {
        this.#consumeLine();
        this.#cr = char === "\r";
      } else {
        this.#line += char;
        if (++this.#size > this.limit) invalid();
      }
    }
  }
  #consumeLine() {
    const line = this.#line;
    this.#line = "";
    if (!line) {
      if (this.#data.length) this.#event(this.#data.join("\n"));
      this.#data = [];
      this.#size = 0;
    } else if (line.startsWith("data:")) {
      this.#data.push(line.slice(5).replace(/^ /, ""));
    }
  }
  #event(data: string) {
    let event: unknown;
    try {
      event = JSON.parse(data);
    } catch {
      invalid();
    }
    if (!object(event) || typeof event.type !== "string") invalid();
    if (["error", "response.failed", "response.incomplete"].includes(event.type)) invalid();
    if (event.type === "response.completed") {
      responsesResult(event.response);
      this.#result = event.response;
      this.done = true;
      this.progress();
    } else if (
      [
        "response.output_text.delta",
        "response.function_call_arguments.delta",
        "response.reasoning_summary_text.delta",
        "response.reasoning_text.delta",
      ].includes(event.type) &&
      typeof event.delta === "string" && event.delta.length > 0
    ) this.progress();
  }
  result(): unknown {
    if (!this.done) invalid();
    return this.#result;
  }
}
