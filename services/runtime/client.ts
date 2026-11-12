import type { OwnershipProof } from "../../contracts/ownership.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";

export interface RuntimeConnection {
  connectionFile: string;
  keyFile: string;
  proofFile: string;
}
export class RuntimeClient {
  #controller = new AbortController();
  constructor(readonly config: RuntimeConnection) {}
  async call(
    namespace: string,
    method: string,
    args: Json = {},
    actionId = `read_${crypto.randomUUID()}`,
  ): Promise<Json> {
    const connection = JSON.parse(await Deno.readTextFile(this.config.connectionFile)),
      url = new URL(connection.url);
    if (
      url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port ||
      url.pathname !== "/runtime"
    ) throw new KernelError("rejected", "执行服务只接受本机连接");
    const proof: OwnershipProof = JSON.parse(await Deno.readTextFile(this.config.proofFile));
    const body = { namespace, method, args, ownership: { proof, actionId } };
    try {
      const response = await fetch(url, {
        method: "POST",
        headers: {
          authorization: `Bearer ${(await Deno.readTextFile(this.config.keyFile)).trim()}`,
          "content-type": "application/json",
        },
        body: JSON.stringify(body),
        signal: AbortSignal.any([this.#controller.signal, AbortSignal.timeout(300000)]),
        redirect: "error",
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unknown", "执行回执未确认，原动作不能重放");
      }
      const reader = response.body.getReader(), chunks = [];
      let size = 0;
      try {
        while (true) {
          const part = await reader.read();
          if (part.done) break;
          size += part.value.length;
          if (size > 16_777_216) {
            await reader.cancel();
            throw new KernelError("unknown", "执行回执超过当前格式");
          }
          chunks.push(part.value);
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
      const result = JSON.parse(new TextDecoder().decode(joined));
      if (result?.ok === true && Object.hasOwn(result, "value")) return result.value;
      if (
        result?.ok === false &&
        [
          "remote",
          "timeout",
          "unavailable",
          "not_found",
          "rejected",
          "corrupt",
          "storage",
          "unknown",
        ].includes(result.error?.kind) && typeof result.error.message === "string"
      ) throw new KernelError(result.error.kind, result.error.message);
      throw new KernelError("unknown", "执行回执无效，查询原动作");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unknown", "执行连接中断，查询原动作");
    }
  }
  close() {
    this.#controller.abort();
  }
}
