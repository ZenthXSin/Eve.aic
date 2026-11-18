import { CHAT_PAGE } from "../services/admin/studio-chat-contract.ts";
import { StudioSettingsError } from "../services/admin/studio-settings.ts";

export const BUDGET = "budget.dailyTokenBudget";
export const INSTANCE = "STUDIO-DEV-01";
export const HISTORY_FIRST = "本地验收 · 消息 01";
export const HISTORY_LAST = `本地验收 · 消息 24 · ${"Unbroken".repeat(36)}`;
const at = (seq) => new Date(Date.UTC(2026, 0, 1, 12, 0, seq)).toISOString();
const uuid = (turn) => `00000000-0000-4000-8000-${String(turn).padStart(12, "0")}`;

export function fixture() {
  const writes = [], queries = [], unexpected = [];
  let budget = 42000, version = 1;
  const messages = Array.from({ length: 24 }, (_, index) => {
    const seq = index + 1, turn = uuid(Math.ceil(seq / 2)), assistant = seq % 2 === 0;
    return {
      seq,
      at: at(seq),
      id: assistant ? `reply:${turn}` : turn,
      role: assistant ? "assistant" : "user",
      status: assistant ? "sent" : "answered",
      ...(assistant ? { replyTo: turn } : {}),
      text: seq === 24 ? HISTORY_LAST : `本地验收 · 消息 ${String(seq).padStart(2, "0")}`,
    };
  });
  const events = Array.from({ length: 24 }, (_, index) => {
    const seq = index + 1, start = seq % 2 === 1;
    return {
      seq,
      at: at(seq),
      kind: start ? "model_start" : "model_response",
      digest: seq.toString(16).padStart(64, "0"),
      data: {
        requestId: `model_${Math.ceil(seq / 2)}`,
        ...(start
          ? { runtimeTiming: { preparationMs: 12, recallMs: 3 } }
          : { response: { content: `开发记录 ${seq}`, toolCalls: [] } }),
      },
    };
  });
  const revision = () => version.toString(16).padStart(64, "0");
  const settings = () => ({
    revision: revision(),
    items: [
      {
        key: BUDGET,
        label: "每日共享 token 预算",
        group: "账本",
        scope: "budget",
        type: "number",
        value: budget,
        min: 0,
        max: 1000000,
        mode: "hot",
        editable: true,
        description: "仅此内存账本。",
      },
      {
        key: "model.session",
        label: "模型会话标识",
        group: "模型",
        scope: "runtime",
        type: "secret",
        mode: "restricted",
        editable: false,
        isSet: true,
        description: "仅展示配置状态。",
      },
    ],
  });
  function reject(label) {
    unexpected.push(label);
    throw new Error(`unexpected fixture operation: ${label}`);
  }
  const backend = {
    async status() {
      return {
        supervisor: { current: INSTANCE, paused: false, upgrade: null },
        ownership: { mode: "active", activePermits: 1, pendingRequest: null },
        runtime: {
          available: true,
          throughSeq: events.length,
          receiptsThroughSeq: 3,
          recovery: { stage: "fixture-ready" },
          model: { blocked: null, current: { model: "fixture-model" } },
          budget: {
            day: at(1).slice(0, 10),
            timeZone: "UTC",
            paused: false,
            dailyTokenBudget: budget,
            knownNoncachedTokens: 120,
            unknownReservedTokens: 0,
            safetyTotal: 120,
            safeRemaining: budget - 120,
          },
        },
      };
    },
    async query({ resource, operation, args }) {
      queries.push({ resource, operation, args });
      if (operation === "page" && resource === "qqFailures") return { items: [] };
      if (operation === "page" && resource === "timeline") {
        const after = args.afterSeq ?? 0, throughSeq = args.throughSeq ?? events.length;
        const items = events.slice(after, Math.min(after + 8, throughSeq));
        return {
          items,
          throughSeq,
          ...(after + items.length < throughSeq
            ? { next: { afterSeq: after + items.length, throughSeq } }
            : {}),
        };
      }
      return reject(`query ${resource}/${operation}`);
    },
    async control({ action }) {
      return reject(`control ${action}`);
    },
    async studio(request) {
      switch (request.action) {
        case "settings":
          return settings();
        case "settingsUpdate": {
          if (request.revision !== revision()) throw new StudioSettingsError("conflict");
          const value = request.changes[BUDGET];
          if (
            Object.keys(request.changes).length !== 1 || !Number.isSafeInteger(value) ||
            value < 0 || value > 1000000
          ) {
            return reject("settings fields");
          }
          budget = value;
          version++;
          writes.push("settingsUpdate");
          return settings();
        }
        case "chat": {
          const all = messages.filter((message) => message.seq < (request.before ?? Infinity));
          const page = all.slice(-CHAT_PAGE);
          return {
            messages: page,
            nextBefore: all.length > page.length ? page[0].seq : null,
            mode: "isolated",
            description: "受控开发私聊夹具；没有模型或 QQ 连接。",
          };
        }
        case "chatSend": {
          writes.push("chatSend");
          const seq = messages.length + 1;
          const message = {
            seq,
            id: request.id,
            role: "user",
            text: request.text,
            status: "queued",
            at: at(seq),
          };
          messages.push(message);
          return { message, mode: "isolated" };
        }
        default:
          return reject(request.action);
      }
    },
  };
  return { backend, writes, queries, unexpected, revision, budget: () => budget };
}
