import type {
  ExitPermitRequest,
  OwnershipGrant,
  OwnershipProof,
  PermitOutcome,
} from "../contracts/ownership.ts";
import { type Json, KernelError, toWireError } from "../kernel/protocol.ts";
import { OwnershipAuthority } from "./ownership.ts";

export function startOwnershipRpc(
  authority: OwnershipAuthority,
  adminKey: string,
  exitKey: string,
  recovery?: { ready(): boolean; progress(): unknown },
) {
  if (
    ![adminKey, exitKey].every((key) => /^[a-zA-Z0-9_-]{32,256}$/.test(key)) || adminKey === exitKey
  ) throw new KernelError("rejected", "守护控制与出口凭据必须独立");
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    if (
      request.method !== "POST" || new URL(request.url).pathname !== "/ownership" ||
      request.headers.has("origin")
    ) return new Response(null, { status: 403 });
    const authorization = request.headers.get("authorization");
    const admin = authorization === `Bearer ${adminKey}`,
      exit = authorization === `Bearer ${exitKey}`;
    try {
      if (!request.body) throw new KernelError("rejected", "请求没有正文");
      const reader = request.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 65_536) {
            await reader.cancel();
            throw new KernelError("rejected", "请求过大");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const body = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        body.set(chunk, offset);
        offset += chunk.length;
      }
      const data = JSON.parse(new TextDecoder().decode(body));
      if (!data || typeof data !== "object" || Array.isArray(data)) {
        throw new KernelError("rejected", "请求格式无效");
      }
      if (recovery && !recovery.ready()) {
        if (data.method === "status" && (admin || exit)) {
          return Response.json({
            ok: true,
            value: { available: false, recovery: recovery.progress() },
          });
        }
        throw new KernelError("unavailable", "守护正在恢复历史记录，尚未开放动作");
      }
      let value: unknown;
      const args = data.args;
      if (data.method === "register" && admin) {
        if (args?.credential === adminKey || args?.credential === exitKey) {
          throw new KernelError("rejected", "实例不能持有守护控制凭据");
        }
        value = await authority.register(args);
      } else if (data.method === "grant" && admin) {
        value = await authority.grant(args as OwnershipGrant);
      } else if (data.method === "pause" && admin) value = await authority.pause(args);
      else if (data.method === "begin" && exit) {
        value = await authority.begin(args as ExitPermitRequest);
      } else if (data.method === "cancel" && exit) {
        await authority.cancel(args?.permit, args?.requestHash);
        value = null;
      } else if (data.method === "finish" && exit) {
        await authority.finish(args?.id, args?.outcome as PermitOutcome, args?.resultHash);
        value = null;
      } else if (data.method === "outstanding" && (admin || exit)) value = authority.outstanding();
      else if (data.method === "status" && (admin || exit)) {
        value = recovery ? { ...authority.status(), available: true } : authority.status();
      } else if (
        (data.method === "renew" || data.method === "actorStatus") &&
        authorization === `Bearer ${args?.credential}`
      ) {
        value = data.method === "renew"
          ? await authority.renew(args as OwnershipProof)
          : await authority.actorStatus(args as OwnershipProof);
      } else throw new KernelError("rejected", "此调用方没有对应守护能力");
      return Response.json({ ok: true, value });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) });
    }
  });
  return { url: `http://127.0.0.1:${server.addr.port}/ownership`, close: () => server.shutdown() };
}

export class OwnershipRpcClient {
  #controller = new AbortController();
  constructor(readonly url: string, readonly key: string, readonly timeoutMs = 10_000) {
    const parsed = new URL(url);
    if (
      parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || !parsed.port ||
      parsed.pathname !== "/ownership" || parsed.username || parsed.password || parsed.search ||
      parsed.hash || !/^[a-zA-Z0-9_-]{32,256}$/.test(key) || !Number.isSafeInteger(timeoutMs) ||
      timeoutMs < 1
    ) throw new KernelError("rejected", "守护连接配置无效");
  }
  async call<T>(method: string, args: unknown = null): Promise<T> {
    try {
      const response = await fetch(this.url, {
        method: "POST",
        headers: { authorization: `Bearer ${this.key}`, "content-type": "application/json" },
        body: JSON.stringify({ method, args }),
        redirect: "error",
        signal: AbortSignal.any([this.#controller.signal, AbortSignal.timeout(this.timeoutMs)]),
      });
      if (response.status !== 200 || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unknown", "守护请求回执不可用，先核对原记录");
      }
      const reader = response.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 131_072) {
            await reader.cancel();
            throw new KernelError("unknown", "守护回执过大，先核对原记录");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const body = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        body.set(chunk, offset);
        offset += chunk.length;
      }
      const value = JSON.parse(new TextDecoder().decode(body));
      if (value?.ok === true && Object.hasOwn(value, "value")) return value.value;
      if (
        value?.ok === false && typeof value.error?.kind === "string" &&
        [
          "remote",
          "timeout",
          "unavailable",
          "not_found",
          "rejected",
          "corrupt",
          "storage",
          "unknown",
        ].includes(value.error.kind) &&
        typeof value.error.message === "string"
      ) throw new KernelError(value.error.kind, value.error.message);
      throw new KernelError("unknown", "守护返回无效，先核对原记录");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      // 请求是否进入守护未知，不能假定没有许可或再次改动号码。
      throw new KernelError("unknown", "守护请求结果未知，请核对原记录");
    }
  }
  close() {
    this.#controller.abort();
  }
}
export class OwnershipExitClient {
  constructor(readonly client: OwnershipRpcClient) {}
  begin(args: ExitPermitRequest) {
    return this.client.call<import("../contracts/ownership.ts").OwnershipPermit>("begin", args);
  }
  async cancel(permit: import("../contracts/ownership.ts").OwnershipPermit, requestHash: string) {
    await this.client.call<Json>("cancel", { permit, requestHash });
  }
  async finish(id: string, outcome: PermitOutcome, resultHash?: string) {
    await this.client.call<Json>("finish", { id, outcome, ...(resultHash ? { resultHash } : {}) });
  }
}
