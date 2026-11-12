import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";
import type { AdminQuery } from "../admin/contracts.ts";
import { ADMIN_READS } from "../admin/contracts.ts";
import { textPage } from "./notes.ts";
import { fingerprint } from "../../kernel/calls.ts";
import type { RuntimeEngine } from "./engine.ts";

export interface RollbackMarker {
  upgradeId: string;
  sourceCommit: string;
  afterSeq: number;
  throughSeq: number;
  reason: string;
}

/** Only the supervisor possesses this capability; instances never receive its key. */
export function startRuntimeAdmin(
  engine: RuntimeEngine,
  key: string,
  quiesce: () => Promise<void>,
) {
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) {
    throw new KernelError("rejected", "执行服务管理凭据无效");
  }
  let tail: Promise<unknown> = Promise.resolve();
  const serial = <T>(run: () => Promise<T>): Promise<T> => {
    const work = tail.then(run);
    tail = work.catch(() => {});
    return work;
  };
  const query = async (request: AdminQuery): Promise<Json> => {
    if (
      !request || request.resource === "qqFailures" || !(request.resource in ADMIN_READS) ||
      !(ADMIN_READS[request.resource] as readonly string[]).includes(request.operation) ||
      !request.args || typeof request.args !== "object" || Array.isArray(request.args)
    ) throw new KernelError("rejected", "管理查询不在契约内");
    const { resource, operation, args } = request;
    if (resource === "notes") {
      if (operation === "page") {
        const result = engine.notes.list({ ...args, includeUnconfirmed: true } as never);
        return { ...result, ...(result.older ? { next: { cursor: result.older } } : {}) };
      }
      if (operation === "search") {
        const result = await engine.notes.search({
          ...args,
          mode: "literal",
          includeUnconfirmed: true,
        } as never);
        return {
          ...result,
          ...(result.hasMore ? { next: { ...args, afterSeq: result.throughSeq } } : {}),
        } as unknown as Json;
      }
      if (operation === "versions") {
        const result = engine.notes.versions(args as never);
        return {
          ...result,
          ...(result.hasMore
            ? { next: { ...args, afterRevision: result.items.at(-1)!.revision } }
            : {}),
        } as unknown as Json;
      }
      return engine.notes.get({ ...args, includeUnconfirmed: true } as never) as unknown as Json;
    }
    if (resource === "memory" && operation === "get") {
      const event = typeof args.id === "string"
        ? engine.memory.journal.events.find((event) =>
          event.kind === "formation" && (event.data as { id: string }).id === args.id
        )
        : Number.isSafeInteger(args.seq) && Number(args.seq) > 0
        ? engine.memory.journal.events[Number(args.seq) - 1]
        : undefined;
      if (!event || event.kind !== "formation") {
        throw new KernelError("not_found", "记忆原件不存在");
      }
      return {
        id: (event.data as { id: string }).id,
        seq: event.seq,
        at: event.at,
        digest: event.digest,
        ...textPage(JSON.stringify(event), args.startChar as number, args.limit as number),
      };
    }
    const journal = resource === "timeline"
      ? engine.records
      : resource === "receipts"
      ? engine.transactions
      : engine.memory.journal;
    if (operation === "get") {
      const seq = args.seq;
      if (!Number.isSafeInteger(seq) || Number(seq) < 1 || Number(seq) > journal.events.length) {
        throw new KernelError("not_found", "原记录不存在");
      }
      const event = journal.events[Number(seq) - 1];
      return {
        seq: event.seq,
        digest: event.digest,
        ...textPage(JSON.stringify(event), args.startChar as number, args.limit as number),
      };
    }
    const after = args.afterSeq ?? 0, through = args.throughSeq ?? journal.events.length;
    if (
      !Number.isSafeInteger(after) || Number(after) < 0 || !Number.isSafeInteger(through) ||
      Number(through) < Number(after) || Number(through) > journal.events.length ||
      operation === "search" &&
        (typeof args.query !== "string" || !args.query || args.query.length > 512)
    ) throw new KernelError("rejected", "管理分页参数无效");
    const items: Json[] = [];
    let cursor = Number(after), scanned = 0;
    while (cursor < Number(through) && items.length < 8 && scanned++ < 256) {
      const event = journal.events[cursor++];
      if (resource === "memory" && event.kind !== "formation") continue;
      if (operation === "search" && !JSON.stringify(event).includes(String(args.query))) continue;
      const data = resource === "memory"
        ? engine.memory.get({ id: (event.data as { id: string }).id }) as unknown as Json
        : event as unknown as Json;
      const encoded = JSON.stringify(data);
      items.push(
        encoded.length <= 16384 ? data : {
          ...(resource === "memory" ? { id: (event.data as { id: string }).id } : {}),
          seq: event.seq,
          at: event.at,
          kind: event.kind,
          digest: event.digest,
          preview: encoded.slice(0, 16384),
          truncated: true,
        },
      );
    }
    return {
      items,
      throughSeq: through,
      ...(cursor < Number(through)
        ? { next: { ...args, afterSeq: cursor, throughSeq: through } }
        : {}),
    };
  };
  const invoke = async (method: string, args: Record<string, Json>): Promise<Json> => {
    if (method === "status") {
      return {
        available: engine.available,
        budget: await engine.budget.status(),
        throughSeq: engine.records.events.length,
        notesThroughSeq: engine.notes.journal.events.length,
        memoryThroughSeq: engine.memory.journal.events.length,
        receiptsThroughSeq: engine.transactions.events.length,
        semanticModel: "TODO",
        model: engine.modelStatus(),
      };
    }
    if (method === "query") return await query(args as unknown as AdminQuery);
    if (method === "resumeModel") {
      return await serial(async () => {
        if (
          Object.keys(args).length !== 1 || typeof args.expectedRequestId !== "string" ||
          !/^model_[1-9][0-9]*$/.test(args.expectedRequestId)
        ) {
          throw new KernelError("rejected", "恢复模型需要当前阻塞请求编号");
        }
        if (
          !(await engine.budget.status()).paused ||
          engine.modelStatus().blocked?.requestId !== args.expectedRequestId
        ) {
          throw new KernelError("rejected", "请先暂停运行，并刷新当前模型阻塞状态");
        }
        await quiesce();
        return await engine.resumeModel(args.expectedRequestId) as unknown as Json;
      });
    }
    if (method === "watermark") return engine.records.events.length;
    if (method === "quiesce") {
      await quiesce();
      return null;
    }
    if (method === "settings") {
      return await serial(async () => {
        if (
          Object.keys(args).some((k) => !["dailyTokenBudget", "paused"].includes(k)) ||
          args.dailyTokenBudget !== undefined &&
            (!Number.isSafeInteger(args.dailyTokenBudget) || Number(args.dailyTokenBudget) < 0) ||
          args.paused !== undefined && typeof args.paused !== "boolean"
        ) {
          throw new KernelError("rejected", "设置只能更改日预算与暂停状态");
        }
        const settings = await engine.budget.settings();
        await writeDurableJson(engine.options.settingsFile, { ...settings, ...args });
        return await engine.budget.status();
      });
    }
    if (method === "rollback") {
      return await serial(async () => {
        const marker = args as unknown as RollbackMarker;
        if (
          !marker || typeof marker.upgradeId !== "string" ||
          !/^[a-zA-Z0-9_-]{1,128}$/.test(marker.upgradeId) ||
          !/^[a-f0-9]{40}$/.test(marker.sourceCommit) || !Number.isSafeInteger(marker.afterSeq) ||
          marker.afterSeq < 0 ||
          !Number.isSafeInteger(marker.throughSeq) || marker.throughSeq < marker.afterSeq ||
          marker.throughSeq > engine.records.events.length ||
          typeof marker.reason !== "string" || !marker.reason || marker.reason.length > 4096
        ) {
          throw new KernelError("rejected", "回退标记需要有效来源与原件范围");
        }
        for (const event of engine.records.events) {
          if (
            event.kind === "upgrade_rollback" &&
            (event.data as { upgradeId: string }).upgradeId === marker.upgradeId
          ) {
            if (await fingerprint(event.data) !== await fingerprint(args)) {
              throw new KernelError("rejected", "回退标记不能改写");
            }
            return event as unknown as Json;
          }
        }
        return await engine.records.append("upgrade_rollback", args) as unknown as Json;
      });
    }
    throw new KernelError("rejected", "未知的执行服务管理方法");
  };
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    if (
      request.method !== "POST" || new URL(request.url).pathname !== "/runtime-control" ||
      request.headers.has("origin") ||
      request.headers.get("authorization") !== `Bearer ${key}`
    ) return new Response(null, { status: 403 });
    try {
      if (!request.body) throw new KernelError("rejected", "缺少管理请求正文");
      const reader = request.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 65_536) {
            await reader.cancel();
            throw new KernelError("rejected", "管理请求过大");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const bytes = new Uint8Array(size);
      let at = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, at);
        at += chunk.length;
      }
      const data = JSON.parse(new TextDecoder().decode(bytes));
      if (
        typeof data?.method !== "string" || !data.args || typeof data.args !== "object" ||
        Array.isArray(data.args)
      ) throw new KernelError("rejected", "管理请求无效");
      return Response.json({ ok: true, value: await invoke(data.method, data.args) });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) });
    }
  });
  return {
    url: `http://127.0.0.1:${server.addr.port}/runtime-control`,
    async close() {
      await server.shutdown();
      await tail;
    },
  };
}

export class RuntimeAdminClient {
  constructor(readonly config: { connectionFile: string; keyFile: string }) {}
  async call<T extends Json>(method: string, args: unknown = {}): Promise<T> {
    const { url } = JSON.parse(await Deno.readTextFile(this.config.connectionFile));
    const parsed = new URL(url);
    if (
      parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || !parsed.port ||
      parsed.pathname !== "/runtime-control" || parsed.username || parsed.password ||
      parsed.search || parsed.hash
    ) throw new KernelError("rejected", "管理出口仅限本机");
    try {
      const response = await fetch(parsed, {
        method: "POST",
        headers: {
          authorization: `Bearer ${(await Deno.readTextFile(this.config.keyFile)).trim()}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ method, args }),
        signal: AbortSignal.timeout(300000),
        redirect: "error",
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unknown", "管理回执未确认");
      }
      const reader = response.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 16_777_216) {
            await reader.cancel();
            throw new KernelError("unknown", "管理回执超过当前格式");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const bytes = new Uint8Array(size);
      let at = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, at);
        at += chunk.length;
      }
      const result = JSON.parse(new TextDecoder().decode(bytes));
      if (result?.ok === true && Object.hasOwn(result, "value")) return result.value;
      if (
        result?.ok === false &&
        [
          "rejected",
          "unknown",
          "storage",
          "corrupt",
          "not_found",
          "unavailable",
          "timeout",
          "remote",
        ].includes(result.error?.kind) && typeof result.error.message === "string"
      ) throw new KernelError(result.error.kind, result.error.message);
      throw new KernelError("unknown", "管理回执无效");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unknown", "管理连接中断，须核对原记录");
    }
  }
  status() {
    return this.call("status");
  }
  query(request: AdminQuery) {
    return this.call("query", request);
  }
  settings(patch: { dailyTokenBudget?: number; paused?: boolean }) {
    return this.call("settings", patch);
  }
  resumeModel(expectedRequestId: string) {
    return this.call("resumeModel", { expectedRequestId });
  }
  quiesce() {
    return this.call("quiesce").then(() => {});
  }
  watermark() {
    return this.call<number>("watermark");
  }
  rollback(marker: RollbackMarker) {
    return this.call("rollback", marker).then(() => {});
  }
}
