import type { RuntimeRequest } from "../../contracts/runtime.ts";
import { fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError, toWireError } from "../../kernel/protocol.ts";
import { OwnershipFence } from "../../sdk/ownership-fence.ts";
import { RuntimeEngine } from "./engine.ts";

export async function startRuntimeRpc(engine: RuntimeEngine, key: string, fence: OwnershipFence) {
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) {
    throw new KernelError("rejected", "执行服务连接凭据无效");
  }
  await fence.reconcile(async (permit) => {
    const receipt = engine.receipt(permit.actionId);
    if (!receipt) {
      return { outcome: "not_executed", resultHash: await fingerprint({ notExecuted: true }) };
    }
    return {
      outcome: receipt.error
        ? receipt.error.kind === "unknown" ? "unknown" : "failed"
        : "completed",
      resultHash: await fingerprint((receipt.error ?? receipt.value!) as unknown as Json),
    };
  });
  const live = new Set<Promise<unknown>>();
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    if (
      request.method !== "POST" || new URL(request.url).pathname !== "/runtime" ||
      request.headers.has("origin") || request.headers.get("authorization") !== `Bearer ${key}`
    ) return new Response(null, { status: 403 });
    try {
      if (!request.body) throw new KernelError("rejected", "请求没有正文");
      const reader = request.body.getReader(), chunks = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 16_777_216 && request.headers.get("x-eve-namespace") !== "agents") {
            await reader.cancel();
            throw new KernelError("rejected", "请求超过当前格式上限");
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
      const data = JSON.parse(new TextDecoder().decode(bytes)) as RuntimeRequest;
      if (request.headers.get("x-eve-namespace") === "agents" && data.namespace !== "agents") {
        throw new KernelError("rejected", "消息传输命名空间不符");
      }
      if (data.namespace === "health") {
        return Response.json({
          ok: true,
          value: { available: engine.available && fence.available, ownershipFenced: true },
        });
      }
      if (
        !data.ownership || typeof data.ownership.actionId !== "string" ||
        !/^[a-zA-Z0-9_-]{1,256}$/.test(data.ownership.actionId) || !data.args ||
        typeof data.args !== "object" || Array.isArray(data.args)
      ) throw new KernelError("rejected", "执行请求需要动作编号、令牌和参数");
      const args = data.args;
      if (data.namespace === "commands" && data.method === "start") {
        const ms = args.waitMs ?? 1000;
        if (!Number.isSafeInteger(ms) || Number(ms) < 0 || Number(ms) > 10000) {
          throw new KernelError("rejected", "命令等待参数无效");
        }
      }
      let admitted!: () => void;
      const admission = new Promise<void>((resolve) => admitted = resolve);
      const work = fence.run(data.ownership.proof, data.ownership.actionId, {
        namespace: data.namespace,
        method: data.method,
        args,
      }, async () => {
        if (data.namespace === "agents" && data.method === "send") {
          await engine.validateRepeat(data.namespace, data.method, args, data.ownership.actionId);
          admitted();
        }
        return await engine.dispatch(data.namespace, data.method, args, data.ownership.actionId);
      });
      live.add(work);
      void work.finally(() => live.delete(work)).catch(() => {});
      if (data.namespace === "agents" && data.method === "send") {
        const controller = new AbortController();
        try {
          const outcome = await Promise.race([
            work.then((value) => ({ value })),
            admission.then(() => engine.agents.accepted(data.ownership.actionId, controller.signal))
              .then(() => null),
          ]);
          return Response.json({
            ok: true,
            value: outcome
              ? outcome.value
              : engine.agents.status({ jobId: data.ownership.actionId }),
          });
        } finally {
          controller.abort();
        }
      }
      if (data.namespace === "commands" && data.method === "start") {
        const ms = args.waitMs ?? 1000;
        const sessionId = await fingerprint(["command-v1", data.ownership.actionId]);
        const controller = new AbortController();
        try {
          await Promise.race([work, engine.commands.started(sessionId, controller.signal)]);
        } finally {
          controller.abort();
        }
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          const result = await Promise.race([
            work.then((value) => ({ value })),
            new Promise<null>((resolve) => timer = setTimeout(() => resolve(null), Number(ms))),
          ]);
          if (result) return Response.json({ ok: true, value: result.value });
          return Response.json({ ok: true, value: await engine.commands.poll({ sessionId }) });
        } finally {
          clearTimeout(timer);
        }
      }
      return Response.json({ ok: true, value: await work });
    } catch (error) {
      return Response.json({ ok: false, error: toWireError(error) });
    }
  });
  return {
    url: `http://127.0.0.1:${server.addr.port}/runtime`,
    async quiesce() {
      // The caller first stops the owning instance. Drain actual work, including a command
      // whose exit-permit request was still in flight when the first cancellation ran.
      await engine.agents.cancelAll();
      while (live.size) {
        await engine.commands.cancelAll();
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          await Promise.race([
            Promise.allSettled([...live]),
            new Promise<void>((resolve) => timer = setTimeout(resolve, 50)),
          ]);
        } finally {
          clearTimeout(timer);
        }
      }
    },
    async close() {
      // 先终结命令，避免 HTTP shutdown 等待长命令形成互等。
      await engine.agents.cancelAll();
      await engine.commands.close();
      await server.shutdown();
      await Promise.allSettled([...live]);
      await fence.close();
      await engine.close();
    },
  };
}
