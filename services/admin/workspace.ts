import type { Json } from "../../kernel/protocol.ts";
import type { AdminBackend } from "./contracts.ts";
import type { LiveEvent } from "./live.ts";
import { observedPreview } from "../../sdk/script-observer.ts";
import { TOOL_CATALOG } from "./tool-catalog.ts";

const object = (v: unknown): Record<string, unknown> =>
  v && typeof v === "object" && !Array.isArray(v) ? v as Record<string, unknown> : {};
const appIds = ["qq", "files", "terminal", "notes", "memory", "browser", "tools"] as const;
const namespaces: Record<string, typeof appIds[number]> = {
  phone: "qq",
  files: "files",
  commands: "terminal",
  notes: "notes",
  memory: "memory",
};

/** Presentation only: no execution, no file path parameters, no phone.open or read acknowledgement. */
export class WorkspaceReader {
  #cached?: Json;
  #readAt = 0;
  #pending?: Promise<Json>;
  constructor(readonly backend: AdminBackend, readonly now = Date.now) {}
  read(): Promise<Json> {
    if (this.#pending) return this.#pending;
    if (this.#cached && this.now() - this.#readAt < 2500) return Promise.resolve(this.#cached);
    const pending = this.#read();
    this.#pending = pending;
    void pending.finally(() => {
      if (this.#pending === pending) this.#pending = undefined;
    }).catch(() => {});
    return pending;
  }
  async #read(): Promise<Json> {
    const [actionsResult, qqResult] = await Promise.allSettled([
      this.backend.query({ resource: "workspace", operation: "page", args: {} }),
      this.backend.query({ resource: "qqConversations", operation: "page", args: {} }),
    ]);
    const journal = actionsResult.status === "fulfilled" ? object(actionsResult.value) : {};
    const operations: Record<string, unknown>[] = Array.isArray(journal.items)
      ? journal.items.map((item) => ({
        ...object(item),
        appId: namespaces[String(object(item).namespace)] ?? "tools",
        source: "script_ledger",
      }))
      : [];
    const latest = operations.at(-1);
    const conversationFor = (item: Record<string, unknown>) => {
      const args = object(item.args);
      const id = args.conversationId ?? object(args.handle).conversationId;
      return typeof id === "string" ? id : null;
    };
    const phoneAction = [...operations].reverse().find((item) =>
      item.namespace === "phone" && conversationFor(item)
    );
    const activeConversationId = phoneAction ? conversationFor(phoneAction) : null;
    const phoneView = [...operations].reverse().find((item) => {
      if (
        item.namespace !== "phone" || item.status !== "completed" ||
        conversationFor(item) !== activeConversationId
      ) return false;
      const result = object(item.result);
      return Array.isArray(result.messages) || Boolean(result.handle && result.body);
    });
    const viewResult = object(phoneView?.result);
    const capturedMessages = Array.isArray(viewResult.messages)
      ? viewResult.messages
      : viewResult.handle && viewResult.body
      ? [viewResult]
      : [];
    const focusArgs = object(phoneAction?.args), focusResult = object(phoneAction?.result);
    const focusedText = phoneAction?.method === "text" && phoneAction.status === "completed" &&
        typeof focusResult.text === "string"
      ? {
        handle: object(focusArgs.handle) as unknown as Json,
        startChar: typeof focusArgs.startChar === "number" ? focusArgs.startChar : 0,
        result: focusResult as Json,
        at: String(phoneAction.startedAt),
        sourceOperationId: String(phoneAction.id),
      }
      : null;
    const qq = qqResult.status === "fulfilled" ? object(qqResult.value) : {};
    const asOf = new Date(this.now()).toISOString();
    const apps: Record<string, Json> = {};
    for (const appId of appIds) {
      const items = appId === "tools"
        ? operations
        : operations.filter((item) => item.appId === appId);
      apps[appId] = {
        available: actionsResult.status === "fulfilled",
        source: "script_ledger",
        asOf,
        items: items as unknown as Json,
        ...(appId === "tools" ? { catalog: TOOL_CATALOG } : {}),
        partial: journal.partial === true,
        ...(!items.length
          ? {
            emptyReason: appId === "browser"
              ? "近期没有结构化浏览器操作记录。"
              : "近期窗口没有此应用的真实操作记录。",
          }
          : {}),
      };
    }
    apps.qq = {
      ...object(apps.qq),
      available: qqResult.status === "fulfilled",
      source: "qq_phone",
      conversations: Array.isArray(qq.items) ? qq.items as Json[] : [],
      activeConversationId,
      activeAt: typeof phoneAction?.startedAt === "string" ? phoneAction.startedAt : null,
      focusedText,
      focusedMessageSeq: typeof object(focusArgs.handle).seq === "number"
        ? object(focusArgs.handle).seq as number
        : null,
      messages: capturedMessages as Json[],
      messageSource: capturedMessages.length ? "eve_script_receipt" : "unavailable",
      viewedAt: phoneView?.startedAt as string ?? null,
      viewOperationId: phoneView?.id as string ?? null,
      snapshotThroughSeq: typeof qq.snapshotThroughSeq === "number" ? qq.snapshotThroughSeq : null,
      partial: true,
      ...(qq.older ? { older: qq.older as Json } : {}),
      ...(!capturedMessages.length
        ? {
          emptyReason: activeConversationId
            ? "本次查看的原始消息屏幕未包含在近期回执中，可手动只读浏览会话。"
            : "近期没有 Eve 查看某个会话的明确操作；可手动只读浏览。",
        }
        : { emptyReason: null }),
    };
    this.#cached = {
      version: 1,
      asOf,
      throughSeq: typeof journal.throughSeq === "number" ? journal.throughSeq : null,
      currentApp: latest
        ? {
          appId: String(latest.appId),
          operation: `${latest.namespace}.${latest.method}`,
          sourceSeq: latest.sourceSeq as number,
          at: latest.startedAt as string,
          status: latest.status as string,
          source: "script_ledger",
          operationId: latest.id as string,
        }
        : null,
      apps,
      operations: operations as unknown as Json,
      partial: journal.partial === true || actionsResult.status === "rejected",
      ...(actionsResult.status === "rejected"
        ? { emptyReason: "操作账本暂时不可读；保留真实时间线，不推断 Eve 正在使用哪个应用。" }
        : {}),
    };
    this.#readAt = this.now();
    return this.#cached;
  }
}

/** Join native tool facts already in the bounded live window; no extra I/O or second journal. */
export function withNativeOperations(workspace: Json, events: LiveEvent[]): Json {
  const screen = object(workspace), native = new Map<string, Record<string, unknown>>();
  for (const event of events) {
    for (const call of event.nativeCalls ?? []) {
      if (!call.id) continue;
      native.set(call.id, {
        id: call.id,
        namespace: "native",
        method: call.name,
        appId: "tools",
        status: "requested",
        at: event.at,
        startedAt: event.at,
        sourceSeq: event.seq,
        source: "timeline",
        sourceResource: "timeline",
        args: call.args,
        hasThought: call.hasThought,
        ...(call.thoughtRecordSeq ? { thoughtRecordSeq: call.thoughtRecordSeq } : {}),
        partial: event.truncated || call.partial,
      });
    }
    if (event.kind === "tool_start" && event.callId) {
      const previous = native.get(event.callId);
      native.set(event.callId, {
        ...previous,
        id: event.callId,
        namespace: "native",
        method: event.tool || previous?.method || "unknown",
        appId: "tools",
        status: "started",
        at: event.at,
        startedAt: event.at,
        sourceSeq: event.seq,
        source: "timeline",
        sourceResource: "timeline",
        args: event.toolArgs ?? previous?.args ?? null,
        hasThought: Boolean(previous?.hasThought || event.toolHasThought),
        partial: event.truncated || event.toolPreviewPartial === true,
      });
    }
    if (event.kind === "tool_result" && event.callId) {
      let operation = native.get(event.callId);
      if (!operation) {
        operation = {
          id: event.callId,
          namespace: "native",
          method: "unknown",
          appId: "tools",
          source: "timeline",
          sourceResource: "timeline",
          sourceSeq: event.seq,
          args: null,
          at: event.at,
          startedAt: event.at,
          missingStart: true,
          partial: true,
        };
        native.set(event.callId, operation);
      }
      const receipt = object(event.toolReceipt);
      operation.status = typeof receipt.status === "string" ? receipt.status : "unknown";
      operation.result = event.toolReceipt ?? null;
      operation.at = event.at;
      operation.receiptSeq = event.seq;
      operation.partial ||= event.toolReceiptPartial === true || event.truncated;
    }
  }
  const scripts = Array.isArray(screen.operations) ? screen.operations.map(object) : [];
  const nativeItems = [...native.values()].sort((a, b) =>
    Date.parse(String(a.startedAt)) - Date.parse(String(b.startedAt))
  ).slice(-32);
  const operations = [...scripts, ...nativeItems];
  operations.sort((a, b) => Date.parse(String(a.startedAt)) - Date.parse(String(b.startedAt)));
  const latest = operations.filter((op) =>
    !(op.namespace === "native" &&
      (op.method === "write_thought" || op.missingStart ||
        op.method === "execute_script" && scripts.length))
  ).at(-1);
  const apps = { ...object(screen.apps) };
  apps.tools = {
    ...object(apps.tools),
    available: true,
    items: operations,
    catalog: TOOL_CATALOG,
    partial: screen.partial === true || native.size > 32 ||
      events.some((event) => Number(event.nativePreviewOmitted) > 0),
    emptyReason: operations.length ? null : "近期没有原生或脚本工具调用记录。",
  };
  return {
    ...screen,
    apps,
    operations,
    currentApp: latest
      ? {
        appId: String(latest.appId),
        operation: `${latest.namespace}.${latest.method}`,
        sourceSeq: Number(latest.sourceSeq),
        source: String(latest.source),
        at: String(latest.startedAt),
        status: String(latest.status),
        operationId: String(latest.id),
      }
      : null,
  } as Json;
}

/** Strip internal join material; public preview details live only in the tool list. */
export function publicLiveEvents(events: LiveEvent[]): Json {
  return events.map(({ nativeCalls: _calls, toolArgs: _args, toolReceipt: _receipt, ...event }) =>
    event
  ) as unknown as Json;
}

/** Last-resort frame budget keeps a large valid history useful with explicit partial previews. */
export function boundLiveSnapshot(value: Json): Json {
  if (new TextEncoder().encode(JSON.stringify(value)).length <= 1_500_000) return value;
  const live = object(value),
    workspace = object(live.workspace),
    apps = { ...object(workspace.apps) };
  const operations: Record<string, unknown>[] =
    (Array.isArray(workspace.operations) ? workspace.operations : []).map(
      (raw) => {
        const operation = object(raw);
        return {
          ...operation,
          args: observedPreview(operation.args, 512).value,
          ...(operation.result === undefined
            ? {}
            : { result: observedPreview(operation.result, 512).value }),
          ...(operation.error === undefined
            ? {}
            : { error: observedPreview(operation.error, 512).value }),
          partial: true,
        };
      },
    );
  for (const [id, raw] of Object.entries(apps)) {
    const app = object(raw);
    apps[id] = {
      ...app,
      items: id === "tools" ? operations : operations.filter((op) => op.appId === id),
      partial: true,
    };
  }
  const events = (Array.isArray(live.events) ? live.events : []).map((raw) => {
    const event = object(raw), text = typeof event.text === "string" ? event.text : "";
    return text.length > 1000
      ? { ...event, text: text.slice(0, 1000) + "\n…", truncated: true }
      : event;
  });
  return {
    ...live,
    events,
    workspace: { ...workspace, operations, apps, partial: true },
    partial: true,
  } as Json;
}
