import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { ModelSelection, type ModelSelectionResult } from "./model-selection.ts";

export { RuntimeModelControlClient, validateModelControlConfig } from "./model-control-client.ts";
export type { RuntimeModelControlConfig } from "./model-control-client.ts";

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
