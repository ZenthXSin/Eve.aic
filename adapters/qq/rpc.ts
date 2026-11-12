// 本机适配器 RPC 服务。独立持有平台客户端，手机插件不读取 QQ 凭据或原始日志。
import { PHONE_METHODS } from "../../contracts/phone-rpc.ts";
import type { PhoneMethod, PhoneRpcResponse } from "../../contracts/phone-rpc.ts";
import { QqActionError } from "./actions.ts";
import { QqPhone, QqPhoneError } from "./phone.ts";
import { QqTimelineError } from "./timeline.ts";
import { KernelError } from "../../kernel/protocol.ts";
import { QqAttachmentDownloadError } from "./downloads.ts";
import type { OwnershipFence } from "../../sdk/ownership-fence.ts";

export function startPhoneRpc(
  phone: QqPhone,
  key: string,
  health: () => unknown = () => ({ available: true }),
  fence?: OwnershipFence,
) {
  if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) throw new TypeError("手机连接密钥格式不正确");
  const authorization = `Bearer ${key}`;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, async (request) => {
    if (request.headers.get("authorization") !== authorization || request.headers.has("origin")) {
      return Response.json({ ok: false, error: { category: "authentication" } }, { status: 403 });
    }
    if (request.method !== "POST" || new URL(request.url).pathname !== "/rpc") {
      return Response.json({ ok: false, error: { category: "request" } }, { status: 400 });
    }
    try {
      if (!request.body) throw new QqPhoneError("request");
      const reader = request.body.getReader();
      const chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 1_048_576) {
            await reader.cancel();
            throw new QqPhoneError("request");
          }
          chunks.push(next.value);
        }
      } finally {
        reader.releaseLock();
      }
      const joined = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) {
        joined.set(chunk, offset);
        offset += chunk.length;
      }
      const data = JSON.parse(new TextDecoder().decode(joined));
      if (data === null || typeof data !== "object" || Array.isArray(data)) {
        throw new QqPhoneError("request");
      }
      if (data.method === "health") {
        const reported = health();
        const value = fence && reported && typeof reported === "object"
          ? {
            ...reported,
            available: (reported as { available?: boolean }).available === true && fence.available,
            ownershipFenced: true,
          }
          : reported;
        return Response.json({ ok: true, value });
      }
      if (!PHONE_METHODS.includes(data.method)) throw new QqPhoneError("request");
      const args = data.args ?? {};
      if (typeof args !== "object" || args === null || Array.isArray(args)) {
        throw new QqPhoneError("request");
      }
      const method = data.method as PhoneMethod;
      // 仅固定契约方法可达，不允许取构造器、内部全量原件或任意成员。
      const invoke = async (checkpoint?: () => Promise<void>) => {
        try {
          if (method === "send") {
            return await phone.send(args as never, { beforeAttempt: checkpoint });
          }
          if (method === "sendFile") {
            return await phone.sendFile(args as never, { beforeAttempt: checkpoint });
          }
          return await (phone[method] as (args: never) => unknown).call(phone, args as never);
        } catch (error) {
          if (
            (error instanceof QqPhoneError || error instanceof QqActionError ||
              error instanceof QqTimelineError) &&
            ["storage", "corrupt", "unknown"].includes(error.category)
          ) {
            throw new KernelError(
              error.category as "storage" | "corrupt" | "unknown",
              "手机执行未确认，原件保留",
            );
          }
          throw error;
        }
      };
      let value: unknown;
      if (fence) {
        if (
          !data.ownership || typeof data.ownership.actionId !== "string" ||
          !/^[a-zA-Z0-9_-]{1,128}$/.test(data.ownership.actionId) ||
          ((method === "send" || method === "sendFile") &&
            data.ownership.actionId !== args.actionId)
        ) throw new QqPhoneError("request");
        // 包括读取与游标更新；旧实例不能继续消费入站消息。号码不交给 QqPhone 业务层。
        value = await fence.run(
          data.ownership.proof,
          data.ownership.actionId,
          { method, args },
          invoke as (
            checkpoint: () => Promise<void>,
          ) => Promise<import("../../kernel/protocol.ts").Json>,
        );
      } else value = await invoke();
      return Response.json({ ok: true, value });
    } catch (error) {
      const category = error instanceof KernelError
        ? (error.kind === "rejected" ? "request" : error.kind)
        : error instanceof QqPhoneError || error instanceof QqActionError ||
            error instanceof QqTimelineError
        ? error.category
        : error instanceof SyntaxError || error instanceof TypeError
        ? "request"
        : "internal";
      const response: PhoneRpcResponse = {
        ok: false,
        error: {
          category,
          ...(error instanceof QqAttachmentDownloadError ? { attachment: error.attachment } : {}),
        },
      };
      return Response.json(response);
    }
  });
  return { url: `http://127.0.0.1:${server.addr.port}/rpc`, close: () => server.shutdown() };
}
