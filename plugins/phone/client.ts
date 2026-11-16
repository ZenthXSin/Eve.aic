import type { Json } from "../../kernel/protocol.ts";
import {
  type PhoneAttachmentFailure,
  phoneAttachmentFailure,
  type PhoneMethod,
  type PhoneRpcResponse,
} from "../../contracts/phone-rpc.ts";
import { KernelError } from "../../sdk/plugin.ts";
import type { OwnershipProof } from "../../contracts/ownership.ts";
import { fingerprint } from "../../kernel/calls.ts";

export class PhoneRpcError extends KernelError {
  readonly attachment?: PhoneAttachmentFailure;
  constructor(readonly category: string, diagnostic?: unknown, method?: string) {
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
      }${method ? `；操作：${method}` : ""}${
        category === "request" ? "；请求参数被拒绝，请核对当前类型契约，不代表手机连接不可用" : ""
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
    if (method === "listSendQueue") {
      if (
        !args || typeof args !== "object" || Array.isArray(args) ||
        Object.keys(args).some((key) =>
          !["conversationId", "afterSequence", "limit"].includes(key)
        ) ||
        args.limit !== undefined &&
          (!Number.isSafeInteger(args.limit) || Number(args.limit) < 1 ||
            Number(args.limit) > 32) ||
        args.afterSequence !== undefined &&
          (!Number.isSafeInteger(args.afterSequence) || Number(args.afterSequence) < 0)
      ) {
        throw new KernelError(
          "rejected",
          "phone.listSendQueue参数无效：limit默认8，范围1..32；afterSequence须为非负整数；只接受conversationId、afterSequence、limit。未派发请求，修正后可继续；下一页直接传返回的next。",
        );
      }
    }
    const captured = structuredClone(args);
    const proof = method === "health" ? undefined : await this.ownershipProof?.();
    const actionId = method === "cancelSend"
      ? `cancel_${await fingerprint(captured)}`
      : (method === "send" || method === "sendFile" || method === "sendBurst") && captured &&
          typeof captured === "object" &&
          !Array.isArray(captured)
      ? captured.actionId
      : `read_${crypto.randomUUID()}`;
    // 最长静默等待可达120秒，给回执留余量；仍受插件关闭/调用中断约束。
    const timeoutMs = method === "waitForMessages"
      ? Math.max(this.timeoutMs, 125_000)
      : this.timeoutMs;
    const signal = AbortSignal.any([this.#controller.signal, AbortSignal.timeout(timeoutMs)]);
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
        throw new PhoneRpcError(result.error.category, result.error.attachment, method);
      }
      throw new PhoneRpcError("invalid_response");
    } catch (error) {
      if (error instanceof PhoneRpcError) {
        if (
          [
            "send",
            "sendFile",
            "sendBurst",
            "cancelSend",
            "open",
            "settings",
            "download",
            "image",
            "file",
          ]
            .includes(method) &&
          ["transport", "invalid_response", "internal"].includes(error.category)
        ) {
          throw new PhoneRpcError("unknown");
        }
        throw error;
      }
      // 请求可能已经到达适配器；发送/打开超时不能声称动作没有执行。
      throw new PhoneRpcError(
        [
            "send",
            "sendFile",
            "sendBurst",
            "cancelSend",
            "open",
            "settings",
            "download",
            "image",
            "file",
          ]
            .includes(method)
          ? "unknown"
          : "transport",
      );
    }
  }

  close(): void {
    this.#controller.abort();
  }
}
