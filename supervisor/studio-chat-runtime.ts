import type { ModelCallOptions, ModelRequest } from "../contracts/model.ts";
import type { Json } from "../kernel/protocol.ts";
import type { RuntimeEngine } from "../services/runtime/engine.ts";
import {
  CHAT_DESCRIPTION,
  CHAT_REPLY_BYTES,
  chatError,
  plainText,
  record,
  UUID4,
} from "../services/admin/studio-chat-contract.ts";
import { StudioChatMailbox } from "./studio-chat.ts";

const prefix = "studio_web_";
type ModelPort = Pick<RuntimeEngine, "dispatch" | "receipt" | "modelStatus">;

export function studioModelOptions(
  actionId: string,
  purpose: unknown,
  maxOutputTokens: number,
): ModelCallOptions | undefined {
  if (!actionId.startsWith(prefix)) return;
  const id = actionId.slice(prefix.length);
  if (
    id.length !== 36 || !UUID4.test(id) || purpose !== "isolated" ||
    !Number.isSafeInteger(maxOutputTokens) || maxOutputTokens < 1
  ) throw chatError("invalid");
  return { sessionId: actionId, maxOutputTokens: Math.min(2048, maxOutputTokens) };
}

function addressedReply(id: string, result: unknown): string | undefined {
  if (
    !record(result) || !Array.isArray(result.toolCalls) || result.toolCalls.length ||
    typeof result.content !== "string" || result.content.length > 65_536
  ) return;
  try {
    const reply: unknown = JSON.parse(result.content);
    if (
      record(reply) && Object.keys(reply).length === 2 && reply.replyTo === id &&
      plainText(reply.text, CHAT_REPLY_BYTES, CHAT_REPLY_BYTES)
    ) return reply.text;
  } catch {
    return;
  }
}

export class StudioChatTurns {
  constructor(private readonly mailbox: StudioChatMailbox, private readonly runtime: ModelPort) {}

  #settle(id: string, result: unknown) {
    const text = addressedReply(id, result);
    this.mailbox.finish(id, text === undefined ? "failed" : "answered", text);
  }

  #reconcile(id: string) {
    const receipt = this.runtime.receipt(`${prefix}${id}`);
    if (
      receipt?.namespace === "model" && receipt.method === "complete" && receipt.terminal &&
      !receipt.error && receipt.value !== undefined
    ) this.#settle(id, receipt.value);
    else this.mailbox.finish(id, "unknown");
  }

  recover() {
    const id = this.mailbox.inFlight();
    if (id) this.#reconcile(id);
  }

  pending() {
    return this.mailbox.queued();
  }

  async offer(slot: string, args: Record<string, Json>): Promise<boolean> {
    if (!record(args) || Object.keys(args).length) throw chatError("invalid");
    const claim = this.mailbox.claim(slot);
    if (!claim.id || !claim.history) return claim.used;
    const { id, history } = claim;
    const request: ModelRequest = {
      messages: [{
        role: "system",
        content:
          `你是 Eve。${CHAT_DESCRIPTION}\n自然地回应眼前这句话；有明确依据就直接答，有歧义才澄清，纠正后采用新事实。不要虚构经历、实时资料、配置或已经执行的动作。\n只把发给 Web 收件人的正文放入一个 JSON 对象：` +
          `{"replyTo":"${id}","text":"正文"}。不要附带推理、工具请求或其他字段。`,
      }, ...history],
      tools: [],
    };
    let result: Json;
    try {
      if (this.runtime.modelStatus().blocked) {
        this.mailbox.finish(id, "failed");
        return true;
      }
      result = await this.runtime.dispatch("model", "complete", {
        messages: request.messages as unknown as Json,
        tools: [],
        _purpose: "isolated",
      }, `${prefix}${id}`);
    } catch {
      this.#reconcile(id);
      return true;
    }
    this.#settle(id, result);
    return true;
  }
}
