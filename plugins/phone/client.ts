import type { Json } from "../../kernel/protocol.ts";
import type { PhoneMethod, PhoneRpcResponse } from "../../contracts/phone-rpc.ts";
import { KernelError } from "../../sdk/plugin.ts";
import type { OwnershipProof } from "../../contracts/ownership.ts";

export class PhoneRpcError extends KernelError {
  constructor(readonly category: string) {
    super(
      category === "unknown"
        ? "unknown"
        : category === "storage"
        ? "storage"
        : category === "corrupt"
        ? "corrupt"
        : category === "unavailable"
        ? "unavailable"
        : category === "not_found"
        ? "not_found"
        : category === "request" || category === "conflict"
        ? "rejected"
        : "remote",
      `手机请求未完成：${category}`,
    );
    this.name = "PhoneRpcError";
  }
}

export class PhoneRpcClient {
  #controller = new AbortController();
  constructor(
    readonly url: string,
    readonly key: string,
    readonly timeoutMs = 120_000,
    readonly ownershipProof?: () => Promise<OwnershipProof>,
  ) {
    const parsed = new URL(url);
    if (
      parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" ||
      !parsed.port || parsed.pathname !== "/rpc" || parsed.username || parsed.password ||
      parsed.search || parsed.hash || !/^[a-zA-Z0-9_-]{32,256}$/.test(key) ||
      !Number.isSafeInteger(timeoutMs) || timeoutMs <= 0
    ) throw new PhoneRpcError("configuration");
  }

  async call(method: PhoneMethod | "health", args: Json = {}): Promise<Json> {
    const captured = structuredClone(args);
    const proof = method === "health" ? undefined : await this.ownershipProof?.();
    const actionId =
      method === "send" && captured && typeof captured === "object" && !Array.isArray(captured)
        ? captured.actionId
        : `read_${crypto.randomUUID()}`;
    const signal = AbortSignal.any([this.#controller.signal, AbortSignal.timeout(this.timeoutMs)]);
    try {
      const response = await fetch(this.url, {
        method: "POST",
        headers: { authorization: `Bearer ${this.key}`, "content-type": "application/json" },
        body: JSON.stringify({
          method,
          args: captured,
          ...(proof ? { ownership: { proof, actionId } } : {}),
        }),
        redirect: "error",
        signal,
      });
      if (response.status !== 200 || !response.body) {
        await response.body?.cancel();
        throw new PhoneRpcError("transport");
      }
      const reader = response.body.getReader();
      const chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const next = await reader.read();
          if (next.done) break;
          size += next.value.length;
          if (size > 131_072) {
            await reader.cancel();
            throw new PhoneRpcError("invalid_response");
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
      const result: PhoneRpcResponse = JSON.parse(new TextDecoder().decode(joined));
      if (result?.ok === true && Object.hasOwn(result, "value")) return result.value;
      if (result?.ok === false && typeof result.error?.category === "string") {
        throw new PhoneRpcError(result.error.category);
      }
      throw new PhoneRpcError("invalid_response");
    } catch (error) {
      if (error instanceof PhoneRpcError) {
        if (
          (method === "send" || method === "open") &&
          ["transport", "invalid_response", "internal"].includes(error.category)
        ) {
          throw new PhoneRpcError("unknown");
        }
        throw error;
      }
      // 请求可能已经到达适配器；发送/打开超时不能声称动作没有执行。
      throw new PhoneRpcError(method === "send" || method === "open" ? "unknown" : "transport");
    }
  }

  close(): void {
    this.#controller.abort();
  }
}
