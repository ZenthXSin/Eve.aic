import type { OwnershipProof } from "../../contracts/ownership.ts";
import { UPGRADE_METHODS } from "../../contracts/upgrades.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";

/** 与执行出口分开的监督接口；只持当前实例证明，不读取管理密钥。 */
export class UpgradeClient {
  #controller = new AbortController();
  constructor(readonly config: { connectionFile: string; proofFile: string }) {}
  async call(method: string, args: Json, actionId: string): Promise<Json> {
    if (typeof actionId !== "string" || !actionId || actionId.length > 256) {
      throw new KernelError("rejected", "升级动作编号无效");
    }
    if (!UPGRADE_METHODS.includes(method as never)) {
      throw new KernelError("rejected", "未知升级能力");
    }
    if (args === null || typeof args !== "object" || Array.isArray(args)) {
      throw new KernelError("rejected", "升级参数必须为对象");
    }
    if (
      method === "request" &&
      (typeof args.commit !== "string" || !/^[a-f0-9]{40}$/.test(args.commit) ||
        Object.keys(args).some((key) => key !== "commit"))
    ) throw new KernelError("rejected", "升级必须指定完整源码提交");
    if (
      method === "status" &&
      (Object.keys(args).some((key) => key !== "actionId") ||
        (args.actionId !== undefined &&
          (typeof args.actionId !== "string" || !args.actionId || args.actionId.length > 256)))
    ) throw new KernelError("rejected", "升级状态参数无效");
    if (
      method === "logs" &&
      (Object.keys(args).some((key) => !["source", "offset", "cursor", "limit"].includes(key)) ||
        (args.source !== undefined && typeof args.source !== "string") ||
        (args.cursor !== undefined && typeof args.cursor !== "string") ||
        (args.offset !== undefined &&
          (!Number.isSafeInteger(args.offset) || Number(args.offset) < 0)) ||
        (args.limit !== undefined && (!Number.isSafeInteger(args.limit) || Number(args.limit) < 1)))
    ) {
      throw new KernelError("rejected", "日志参数无效");
    }
    let url: URL;
    let proof: OwnershipProof;
    try {
      const connection = JSON.parse(await Deno.readTextFile(this.config.connectionFile));
      url = new URL(connection.url);
      proof = JSON.parse(await Deno.readTextFile(this.config.proofFile));
      if (
        typeof proof?.instanceId !== "string" || !proof.instanceId ||
        !Number.isSafeInteger(proof.epoch) || proof.epoch < 1 ||
        typeof proof.credential !== "string" || !proof.credential
      ) throw new Error();
    } catch {
      // JSON 解析错误的原文可能包含实例凭据，绝不进入脚本回执。
      throw new KernelError("unavailable", "升级连接或实例证明未就绪");
    }
    if (
      url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port ||
      url.pathname !== "/upgrade" || url.username || url.password || url.search || url.hash
    ) throw new KernelError("rejected", "升级只接受本机监督接口");
    try {
      const response = await fetch(url, {
        method: "POST",
        headers: {
          authorization: `Bearer ${proof.credential}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({ method, args, ownership: { proof, actionId } }),
        signal: AbortSignal.any([this.#controller.signal, AbortSignal.timeout(30000)]),
        redirect: "error",
      });
      if (!response.ok || !response.body) {
        await response.body?.cancel();
        throw new KernelError("unknown", "升级回执未确认，请查询原动作");
      }
      const reader = response.body.getReader();
      const chunks: Uint8Array[] = [];
      let size = 0;
      try {
        while (true) {
          const part = await reader.read();
          if (part.done) break;
          size += part.value.length;
          if (method !== "logs" && size > 1_048_576) {
            await reader.cancel();
            throw new KernelError("unknown", "升级回执过大，请查询原动作");
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
      const result = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(joined));
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
        ]
          .includes(result.error?.kind) &&
        typeof result.error.message === "string"
      ) throw new KernelError(result.error.kind, result.error.message);
      throw new KernelError("unknown", "升级回执无效，请查询原动作");
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unknown", "升级连接中断，请查询原动作");
    }
  }
  close() {
    this.#controller.abort();
  }
}
