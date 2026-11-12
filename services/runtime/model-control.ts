import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { ModelSelection, type ModelSelectionResult } from "./model-selection.ts";

export interface RuntimeModelControlConfig {
  connectionFile: string;
  keyFile: string;
  ownerPlatformUserId: string;
}
export function validateModelControlConfig(
  value: unknown,
): asserts value is RuntimeModelControlConfig {
  const c = value as RuntimeModelControlConfig;
  if (
    !c || typeof c.connectionFile !== "string" || !c.connectionFile.startsWith("/") ||
    typeof c.keyFile !== "string" || !c.keyFile.startsWith("/") ||
    typeof c.ownerPlatformUserId !== "string" || !c.ownerPlatformUserId ||
    c.ownerPlatformUserId.length > 512
  ) throw new KernelError("rejected", "模型控制配置无效");
}
export function startRuntimeModelControl(
  selection: ModelSelection,
  key: string,
  ownerPlatformUserId: string,
  onSelected?: (result: ModelSelectionResult) => Promise<void>,
) {
  if (
    !/^[a-zA-Z0-9_-]{32,256}$/.test(key) || !ownerPlatformUserId ||
    ownerPlatformUserId.length > 512
  ) throw new KernelError("rejected", "模型控制凭据或主人绑定无效");
  let origin = "", host = "";
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    const url = new URL(request.url);
    if (
      request.method !== "POST" || url.origin !== origin ||
      request.headers.get("host") !== host || url.pathname !== "/runtime-model-control" ||
      url.username || url.password || url.search || url.hash ||
      request.headers.has("origin") ||
      ["cross-site", "same-site"].includes(request.headers.get("sec-fetch-site") ?? "") ||
      request.headers.get("authorization") !== `Bearer ${key}`
    ) {
      return new Response(null, { status: 403 });
    }
    try {
      if (!request.body) throw new KernelError("rejected", "缺少模型命令正文");
      const reader = request.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          size += value.length;
          if (size > 4096) {
            await reader.cancel();
            throw new KernelError("rejected", "模型命令过大");
          }
          chunks.push(value);
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
      const { method, args } = JSON.parse(new TextDecoder().decode(bytes));
      if (
        !args || typeof args !== "object" || Array.isArray(args) ||
        args.ownerPlatformUserId !== ownerPlatformUserId
      ) {
        throw new KernelError("rejected", "只有明确绑定的主人可以操作模型");
      }
      const allowed = method === "select"
        ? ["ownerPlatformUserId", "actionId", "model"]
        : method === "receipt"
        ? ["ownerPlatformUserId", "actionId"]
        : ["ownerPlatformUserId"];
      if (Object.keys(args).some((k) => !allowed.includes(k))) {
        throw new KernelError("rejected", "模型命令包含未知参数");
      }
      let value: unknown;
      if (method === "status") value = await selection.status();
      else if (method === "select" || method === "receipt") {
        const result = method === "select"
          ? await selection.select(args.actionId, args.model)
          : selection.receipt(args.actionId);
        await onSelected?.(result);
        value = result;
      } else throw new KernelError("rejected", "未知模型控制方法");
      return Response.json({ ok: true, value });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) });
    }
  });
  origin = `http://127.0.0.1:${server.addr.port}`;
  host = new URL(origin).host;
  return {
    url: `${origin}/runtime-model-control`,
    close: () => server.shutdown(),
  };
}
export class RuntimeModelControlClient {
  constructor(readonly config: RuntimeModelControlConfig) {
    validateModelControlConfig(config);
  }
  async #call<T>(method: string, args: Record<string, Json> = {}): Promise<T> {
    try {
      const { url } = JSON.parse(await Deno.readTextFile(this.config.connectionFile));
      const parsed = new URL(url);
      if (
        parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || !parsed.port ||
        parsed.pathname !== "/runtime-model-control" || parsed.username || parsed.password ||
        parsed.search || parsed.hash
      ) throw new KernelError("rejected", "模型控制仅限本机");
      const response = await fetch(parsed, {
        method: "POST",
        redirect: "error",
        signal: AbortSignal.timeout(25_000),
        headers: {
          "content-type": "application/json",
          authorization: `Bearer ${(await Deno.readTextFile(this.config.keyFile)).trim()}`,
        },
        body: JSON.stringify({
          method,
          args: { ...args, ownerPlatformUserId: this.config.ownerPlatformUserId },
        }),
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unknown", "模型命令回执未确认，请核对原编号");
      }
      const reader = response.body.getReader(), chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          size += value.length;
          if (size > 1_048_576) {
            await reader.cancel();
            throw new KernelError("unknown", "模型回执过大");
          }
          chunks.push(value);
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
      if (result?.ok === true && Object.hasOwn(result, "value")) {
        const v = result.value;
        const validRevision = (r: unknown) => Number.isSafeInteger(r) && Number(r) >= 0;
        const validModel = (m: unknown) => typeof m === "string" && m.length > 0 && m.length <= 256;
        const valid = method === "status"
          ? v && validModel(v.current?.model) && validRevision(v.current?.revision) &&
            Array.isArray(v.availableModels) && v.availableModels.every(validModel)
          : v && v.actionId === args.actionId && validModel(v.model) &&
            (method !== "select" || v.model === args.model) && validRevision(v.revision) &&
            typeof v.changed === "boolean";
        if (!valid) throw new KernelError("unknown", "模型命令回执内容无效，请核对原编号");
        return v;
      }
      if (
        result?.ok === false &&
        ["rejected", "not_found", "unknown", "unavailable", "storage", "corrupt"].includes(
          result.error?.kind,
        ) && typeof result.error.message === "string"
      ) {
        throw new KernelError(result.error.kind, result.error.message);
      }
      throw new KernelError("unknown", "模型命令回执无效，请核对原编号");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unknown", "模型控制连接中断，请核对原编号");
    }
  }
  status() {
    return this.#call<{ current: { model: string; revision: number }; availableModels: string[] }>(
      "status",
    );
  }
  select(actionId: string, model: string) {
    return this.#call<ModelSelectionResult>("select", { actionId, model });
  }
  receipt(actionId: string) {
    return this.#call<ModelSelectionResult>("receipt", { actionId });
  }
}
