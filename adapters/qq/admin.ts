import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { readAdminBody } from "../../services/admin/server.ts";
import { type ResolveFailedQq, validateFailedResolution } from "../../services/admin/contracts.ts";
import { QqActionError, type QqSendActions } from "./actions.ts";
import { type QqPhone, QqPhoneError } from "./phone.ts";

export type QqAdminConversations = NonNullable<Parameters<QqPhone["conversations"]>[0]>;
export type QqAdminHistory = Parameters<QqPhone["history"]>[0];
type ReadOnlyPhone = Pick<QqPhone, "conversations" | "history">;

function fields(value: unknown, allowed: string[]): asserts value is Record<string, unknown> {
  if (
    !value || typeof value !== "object" || Array.isArray(value) ||
    Object.keys(value).some((key) => !allowed.includes(key))
  ) {
    throw new KernelError("rejected", "QQ 只读查询参数无效");
  }
}
function pageCursor(value: unknown, conversations: boolean): void {
  const before = conversations ? "beforeLastSeq" : "beforeSeq";
  fields(value, ["snapshotThroughSeq", before, ...(conversations ? ["beforePinned"] : [])]);
  if (
    !Number.isSafeInteger(value.snapshotThroughSeq) || Number(value.snapshotThroughSeq) < 0 ||
    !Number.isSafeInteger(value[before]) || Number(value[before]) < 1 ||
    Number(value[before]) > Number(value.snapshotThroughSeq) ||
    (Object.hasOwn(value, "beforePinned") && typeof value.beforePinned !== "boolean")
  ) {
    throw new KernelError("rejected", "QQ 只读查询游标无效");
  }
}
function validatePhoneQuery(args: unknown, conversations: boolean): void {
  fields(args, conversations ? ["cursor"] : ["conversationId", "cursor"]);
  if (
    !conversations && (typeof args.conversationId !== "string" ||
      !/^qq:(user|group):[^\r\n]{1,512}$/.test(args.conversationId))
  ) {
    throw new KernelError("rejected", "QQ 会话标识无效");
  }
  if (Object.hasOwn(args, "cursor")) pageCursor(args.cursor, conversations);
}

/** 独立管理能力：失败队列控制与有界会话只读视图；手机/模型不持有此密钥。 */
export function startQqAdminServer(actions: QqSendActions, key: string, phone?: ReadOnlyPhone) {
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) throw new KernelError("rejected", "QQ 管理密钥无效");
  let origin = "";
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const url = new URL(request.url);
    if (
      request.method !== "POST" || url.origin !== origin || url.pathname !== "/qq-admin-control" ||
      url.search !== "" ||
      request.headers.get("host") !== new URL(origin).host || request.headers.has("origin") ||
      ["cross-site", "same-site"].includes(request.headers.get("sec-fetch-site") ?? "") ||
      request.headers.get("authorization") !== `Bearer ${key}`
    ) return new Response(null, { status: 403, headers: { "cache-control": "no-store" } });
    try {
      const body = await readAdminBody(request);
      if (!body.args || typeof body.args !== "object" || Array.isArray(body.args)) {
        throw new KernelError("rejected", "QQ 管理参数无效");
      }
      let value: unknown;
      if (body.method === "failedHeads") {
        if (Object.keys(body.args).some((field) => !["afterSequence", "limit"].includes(field))) {
          throw new KernelError("rejected", "QQ 管理分页参数无效");
        }
        const page = await actions.listSendQueue({ ...body.args, failedHeadsOnly: true } as never);
        value = {
          ...page,
          ...(page.next
            ? { next: { afterSequence: page.next.afterSequence, limit: page.next.limit } }
            : {}),
        };
      } else if (body.method === "resolveFailed") {
        const args = validateFailedResolution(body.args);
        value = await actions.cancelSend({
          actionId: args.expectedActionId,
          expectedFingerprint: args.expectedFingerprint,
          expectedReceiptDigest: args.expectedReceiptDigest,
        }, { actor: "admin", failedHeadOnly: true });
      } else if (body.method === "conversations" || body.method === "history") {
        const conversations = body.method === "conversations";
        validatePhoneQuery(body.args, conversations);
        if (!phone) throw new KernelError("unavailable", "QQ 只读会话查询尚未启用");
        // 与 Eve 使用同一个手机事实投影；history 不调用 open，不移动通知或阅读水位。
        value = conversations
          ? await phone.conversations(body.args as QqAdminConversations)
          : await phone.history(body.args as unknown as QqAdminHistory);
        // Match the client's encoded-byte ceiling. Oversized original identities/cursors
        // cannot be truncated safely; return a small explicit error without skipping records.
        const encoded = JSON.stringify({ ok: true, value });
        if (new TextEncoder().encode(encoded).length > 131_072) {
          throw new KernelError("unavailable", "QQ 只读分页超过字节上限，原始记录保留");
        }
        return new Response(encoded, {
          headers: { "content-type": "application/json", "cache-control": "no-store" },
        });
      } else throw new KernelError("rejected", "未知 QQ 管理操作");
      return Response.json({ ok: true, value }, { headers: { "cache-control": "no-store" } });
    } catch (error) {
      const normalized = error instanceof QqActionError
        ? new KernelError(
          error.category === "request" || error.category === "conflict"
            ? "rejected"
            : error.category,
          "QQ 队列状态已变或不可操作，请刷新核对；原件保留",
        )
        : error instanceof QqPhoneError
        ? new KernelError("rejected", "QQ 只读查询无效，请刷新分页")
        : error;
      return Response.json({ ok: false, error: toWireError(normalized) }, {
        headers: { "cache-control": "no-store" },
      });
    }
  });
  origin = `http://127.0.0.1:${server.addr.port}`;
  return { url: `${origin}/qq-admin-control`, close: () => server.shutdown() };
}

export class QqAdminClient {
  constructor(readonly config: { connectionFile: string; keyFile: string }) {}
  failedHeads(args: Record<string, Json> = {}) {
    return this.#call("failedHeads", args);
  }
  resolveFailed(args: ResolveFailedQq) {
    return this.#call("resolveFailed", args);
  }
  conversations(args: QqAdminConversations = {}) {
    return this.#call("conversations", args);
  }
  history(args: QqAdminHistory) {
    return this.#call("history", args);
  }
  async #call(method: string, args: unknown): Promise<Json> {
    try {
      const { url } = JSON.parse(await Deno.readTextFile(this.config.connectionFile));
      const parsed = new URL(url);
      if (
        parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || !parsed.port ||
        parsed.pathname !== "/qq-admin-control" || parsed.username || parsed.password ||
        parsed.hash || parsed.search
      ) throw new KernelError("rejected", "QQ 管理仅使用本机独立连接");
      const key = (await Deno.readTextFile(this.config.keyFile)).trim();
      if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) {
        throw new KernelError("rejected", "QQ 管理密钥无效");
      }
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
        body: JSON.stringify({ method, args }),
        redirect: "error",
        signal: AbortSignal.timeout(30_000),
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unavailable", "QQ 管理服务不可用");
      }
      const reader = response.body.getReader();
      let size = 0;
      const chunks: Uint8Array[] = [];
      try {
        for (;;) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 131_072) {
            await reader.cancel();
            throw new KernelError("unknown", "QQ 管理结果超过分页上限");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.length;
      }
      const result = JSON.parse(new TextDecoder().decode(bytes));
      if (result?.ok === true && Object.hasOwn(result, "value")) return result.value;
      if (result?.ok === false) {
        throw new KernelError(
          result.error?.kind ?? "unknown",
          result.error?.message ?? "QQ 管理结果未确认",
        );
      }
      throw new KernelError("unknown", "QQ 管理结果未确认");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unknown", "QQ 管理连接中断，请先刷新核对；不自动重发操作");
    }
  }
}
