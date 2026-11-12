import { type Json, KernelError } from "../kernel/protocol.ts";

/** 控制凭据由监督进程持有，绝不传给在线插件或 execute_script。 */
export class InstanceClient {
  #abort = new AbortController();
  constructor(
    readonly connectionFile: string,
    readonly keyFile: string,
    readonly timeoutMs = 180_000,
  ) {}
  async call<T = Json>(method: string, args: Json = {}): Promise<T> {
    try {
      const { url } = JSON.parse(await Deno.readTextFile(this.connectionFile));
      const parsed = new URL(url);
      if (
        parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || !parsed.port ||
        parsed.pathname !== "/instance" || parsed.username || parsed.password || parsed.search ||
        parsed.hash
      ) throw new KernelError("rejected", "实例只接受本机控制连接");
      const response = await fetch(url, {
        method: "POST",
        headers: {
          authorization: `Bearer ${(await Deno.readTextFile(this.keyFile)).trim()}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ method, args }),
        redirect: "error",
        signal: AbortSignal.any([this.#abort.signal, AbortSignal.timeout(this.timeoutMs)]),
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unknown", "实例控制回执未知，先核对状态");
      }
      const reader = response.body.getReader();
      const chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const part = await reader.read();
          if (part.done) break;
          size += part.value.length;
          if (size > 16_777_216) {
            await reader.cancel();
            throw new KernelError("unknown", "实例控制回执超过格式上限");
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
      const value = JSON.parse(new TextDecoder().decode(joined));
      if (value?.ok === true && Object.hasOwn(value, "value")) return value.value;
      const kinds = [
        "remote",
        "timeout",
        "unavailable",
        "not_found",
        "rejected",
        "corrupt",
        "storage",
        "unknown",
      ];
      if (
        value?.ok === false && kinds.includes(value.error?.kind) &&
        typeof value.error.message === "string"
      ) {
        throw new KernelError(value.error.kind, value.error.message);
      }
      throw new KernelError("unknown", "实例控制回执格式无效");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unknown", "实例控制连接中断，不能假定动作未执行");
    }
  }
  close() {
    this.#abort.abort();
  }
}
