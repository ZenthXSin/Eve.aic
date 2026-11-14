import { ChatModelError } from "./errors.ts";

const encoder = new TextEncoder();

function invalid(): never {
  throw new ChatModelError("invalid_response");
}
function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
interface Call {
  id: string;
  type: string;
  function: { name: string; arguments: string };
}

/** Incremental SSE framing and Chat delta assembly; never exposes unfinished tool calls. */
export class ChatStream {
  done = false;
  #line = "";
  #cr = false;
  #data: string[] = [];
  #eventSize = 0;
  #retained = 0;
  #content: string[] = [];
  #refusal: string[] = [];
  #calls = new Map<number, Call>();
  #finish: string | undefined;
  #usage: unknown;
  constructor(readonly limit: number, readonly progress: () => void) {}

  feed(text: string) {
    for (const char of text) {
      if (this.done) {
        if (char.trim()) invalid();
        continue;
      }
      if (this.#cr) {
        this.#cr = false;
        if (char === "\n") continue;
      }
      if (char === "\r" || char === "\n") {
        this.#consumeLine();
        this.#cr = char === "\r";
      } else {
        this.#line += char;
        if (++this.#eventSize > this.limit) invalid();
      }
    }
  }

  #consumeLine() {
    const line = this.#line;
    this.#line = "";
    if (!line) {
      if (this.#data.length) this.#event(this.#data.join("\n"));
      this.#data = [];
      this.#eventSize = 0;
      return;
    }
    if (line.startsWith(":")) return;
    const colon = line.indexOf(":");
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "data") this.#data.push(value);
  }

  #string(value: unknown): string {
    if (typeof value !== "string") invalid();
    // This also bounds ignored reasoning text; it must not accumulate in memory.
    this.#retained += encoder.encode(value).length;
    if (this.#retained > this.limit) invalid();
    return value;
  }

  #event(data: string) {
    if (data === "[DONE]") {
      if (!this.#finish) invalid();
      this.done = true;
      return;
    }
    let chunk: unknown;
    try {
      chunk = JSON.parse(data);
    } catch {
      invalid();
    }
    if (!object(chunk) || chunk.error !== undefined || !Array.isArray(chunk.choices)) invalid();
    if (chunk.usage !== undefined && chunk.usage !== null) this.#usage = chunk.usage;
    if (!chunk.choices.length) return; // include_usage sends a final choices: [] chunk.
    if (chunk.choices.length !== 1 || this.#finish) invalid();
    const choice = chunk.choices[0];
    if (!object(choice) || choice.index !== 0 || !object(choice.delta)) invalid();
    const delta = choice.delta;
    if (delta.role !== undefined && delta.role !== "assistant") invalid();
    let advanced = false;
    for (const [key, target] of [["content", this.#content], ["refusal", this.#refusal]] as const) {
      if (delta[key] !== undefined && delta[key] !== null) {
        const value = this.#string(delta[key]);
        if (value) {
          target.push(value);
          advanced = true;
        }
      }
    }
    for (const key of ["reasoning_content", "reasoning", "reasoning_text"]) {
      if (delta[key] !== undefined && delta[key] !== null) {
        if (this.#string(delta[key])) advanced = true;
      }
    }
    if (delta.tool_calls !== undefined) {
      if (!Array.isArray(delta.tool_calls)) invalid();
      for (const part of delta.tool_calls) {
        if (
          !object(part) || !Number.isSafeInteger(part.index) || Number(part.index) < 0 ||
          Number(part.index) > this.limit
        ) invalid();
        const index = Number(part.index);
        let call = this.#calls.get(index);
        if (!call) {
          call = { id: "", type: "function", function: { name: "", arguments: "" } };
          this.#calls.set(index, call);
        }
        if (part.type !== undefined && part.type !== "function") invalid();
        if (part.id !== undefined) {
          const value = this.#string(part.id);
          call.id += value;
          advanced ||= !!value;
        }
        if (part.function !== undefined) {
          if (!object(part.function)) invalid();
          for (const key of ["name", "arguments"] as const) {
            if (part.function[key] !== undefined) {
              const value = this.#string(part.function[key]);
              call.function[key] += value;
              advanced ||= !!value;
            }
          }
        }
      }
    }
    if (choice.finish_reason !== null && choice.finish_reason !== undefined) {
      if (!["stop", "tool_calls"].includes(String(choice.finish_reason))) invalid();
      this.#finish = String(choice.finish_reason);
      advanced = true;
    }
    // Heartbeats/role-only deltas cannot keep a stuck request alive forever.
    if (advanced) this.progress();
  }

  result(): unknown {
    if (!this.done || !this.#finish) invalid();
    const calls = [...this.#calls].sort(([a], [b]) => a - b);
    if (calls.some(([index], position) => index !== position)) invalid();
    return {
      choices: [{
        finish_reason: this.#finish,
        message: {
          role: "assistant",
          content: this.#content.join("") || (this.#refusal.length ? null : ""),
          refusal: this.#refusal.join(""),
          tool_calls: calls.map(([, call]) => call),
        },
      }],
      ...(this.#usage === undefined ? {} : { usage: this.#usage }),
    };
  }
}
