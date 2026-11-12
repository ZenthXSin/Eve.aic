import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { readAdminBody } from "../../services/admin/server.ts";
import { type ResolveFailedQq, validateFailedResolution } from "../../services/admin/contracts.ts";
import { QqActionError, type QqSendActions } from "./actions.ts";

/** 专用管理能力：只列出失败队首及明确结束失败项；手机/模型不持有此密钥。 */
export function startQqAdminServer(actions: QqSendActions, key: string) {
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
