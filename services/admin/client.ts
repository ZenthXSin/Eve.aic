import type { Json } from "../../kernel/protocol.ts";
import { KernelError } from "../../kernel/protocol.ts";
import type { AdminBackend, AdminControl, AdminQuery, AdminRequest } from "./contracts.ts";

/** 仅管理进程读取此密钥。管理服务不持有 Eve 的所有权令牌或模型密钥。 */
export class AdminControlClient implements AdminBackend {
  constructor(
    readonly connectionFile: string,
    readonly keyFile: string,
    readonly recoveryFiles: string[] = [],
  ) {}
  async status() {
    try {
      return await this.request({ method: "status" });
    } catch (error) {
      // Only status can fall back to private startup projections. Never repeat a mutation.
      if (!(error instanceof KernelError) || !["unknown", "unavailable"].includes(error.kind)) {
        throw error;
      }
      const services: Json[] = [];
      let fresh = false;
      for (const path of this.recoveryFiles) {
        try {
          const value = JSON.parse(await Deno.readTextFile(path));
          if (
            !value || !["runtime", "guardian"].includes(value.service) ||
            !["recovering", "ready", "failed"].includes(value.phase) ||
            !Number.isSafeInteger(value.pid) || value.pid < 1 ||
            !Number.isSafeInteger(value.updatedAt)
          ) continue;
          Deno.kill(value.pid, 0);
          if (value.updatedAt <= Date.now() && Date.now() - value.updatedAt < 30_000) fresh = true;
          services.push(value);
        } catch {
          /* Missing, expired or incomplete startup projections are not health evidence. */
        }
      }
      if (!fresh || !services.length) throw error;
      return { runtime: { available: false }, startup: { phase: "recovering", services } };
    }
  }
  query(request: AdminQuery) {
    return this.request({ method: "query", request });
  }
  control(request: AdminControl) {
    return this.request({ method: "control", request });
  }
  async request(request: AdminRequest): Promise<Json> {
    try {
      return await this.#request(request);
    } catch (error) {
      if (error instanceof KernelError) throw error;
      // 连接中断可能发生在控制操作已提交以后；调用方先查状态，不自动重发。
      throw new KernelError("unknown", "管理连接中断，操作结果尚未确认，请刷新状态核对");
    }
  }
  async #request(request: AdminRequest): Promise<Json> {
    const { url } = JSON.parse(await Deno.readTextFile(this.connectionFile));
    const parsed = new URL(url);
    if (
      parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1" || parsed.username ||
      parsed.password || parsed.search || parsed.hash || parsed.pathname !== "/admin-control"
    ) throw new KernelError("corrupt", "管理后端连接只允许本机管理出口");
    const key = (await Deno.readTextFile(this.keyFile)).trim();
    if (!/^[a-zA-Z0-9_-]{32,256}$/.test(key)) throw new KernelError("corrupt", "管理后端凭据无效");
    const response = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
      body: JSON.stringify(request),
      signal: AbortSignal.timeout(30_000),
      redirect: "error",
    });
    if (!response.ok) {
      await response.body?.cancel();
      throw new KernelError("unavailable", "管理后端不可用");
    }
    const reader = response.body?.getReader();
    if (!reader) throw new KernelError("unknown", "管理后端未返回结果");
    const chunks: Uint8Array[] = [];
    let size = 0;
    try {
      while (true) {
        const next = await reader.read();
        if (next.done) break;
        size += next.value.length;
        if (size > 1_048_576) {
          await reader.cancel();
          throw new KernelError("unknown", "管理响应超过分页上限");
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
    const result = JSON.parse(new TextDecoder().decode(bytes));
    if (!result.ok) {
      throw new KernelError(
        result.error?.kind ?? "unknown",
        result.error?.message ?? "管理操作结果未确认",
      );
    }
    return result.value;
  }
}
