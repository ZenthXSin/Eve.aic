import type { Json } from "../../kernel/protocol.ts";
import {
  type PhoneAttachmentFailure,
  phoneAttachmentFailure,
  type PhoneMethod,
  type PhoneRpcResponse,
} from "../../contracts/phone-rpc.ts";
import { KernelError } from "../../sdk/plugin.ts";
import type { OwnershipProof } from "../../contracts/ownership.ts";

export class PhoneRpcError extends KernelError {
  readonly attachment?: PhoneAttachmentFailure;
  constructor(readonly category: string, diagnostic?: unknown) {
    const attachment = phoneAttachmentFailure(diagnostic);
    super(
      category === "unknown"
        ? "unknown"
        : category === "timeout"
        ? "timeout"
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
      `手机请求未完成：${category}${
        attachment
          ? `；附件下载：${attachment.reason}${
            attachment.httpStatus === undefined ? "" : ` (HTTP ${attachment.httpStatus})`
          }`
          : ""
      }`,
    );
    this.attachment = attachment;
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
    readonly resolveUrl?: () => Promise<string>,
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
      (method === "send" || method === "sendFile") && captured && typeof captured === "object" &&
        !Array.isArray(captured)
        ? captured.actionId
        : `read_${crypto.randomUUID()}`;
    const signal = AbortSignal.any([this.#controller.signal, AbortSignal.timeout(this.timeoutMs)]);
    try {
      let url = this.url;
      if (this.resolveUrl) {
        try {
          url = await this.resolveUrl();
          const parsed = new URL(url);
          if (
            parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || !parsed.port ||
            parsed.pathname !== "/rpc" || parsed.username || parsed.password || parsed.search ||
            parsed.hash
          ) throw new Error();
        } catch {
          throw new PhoneRpcError("unavailable");
        }
      }
      const response = await fetch(url, {
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
          const limit = method === "image"
            ? 16_777_216
            : ["file", "settings"].includes(method)
            ? 1_048_576
            : 131_072;
          if (size > limit) {
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
        throw new PhoneRpcError(result.error.category, result.error.attachment);
      }
      throw new PhoneRpcError("invalid_response");
    } catch (error) {
      if (error instanceof PhoneRpcError) {
        if (
          ["send", "sendFile", "open", "settings", "download", "image", "file"].includes(method) &&
          ["transport", "invalid_response", "internal"].includes(error.category)
        ) {
          throw new PhoneRpcError("unknown");
        }
        throw error;
      }
      // 请求可能已经到达适配器；发送/打开超时不能声称动作没有执行。
      throw new PhoneRpcError(
        ["send", "sendFile", "open", "settings", "download", "image", "file"].includes(method)
          ? "unknown"
          : "transport",
      );
    }
  }

  close(): void {
    this.#controller.abort();
  }
}
