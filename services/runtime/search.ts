import { KernelError } from "../../kernel/protocol.ts";
import type { SearchQuery, SearchResult, SearchStatus } from "../../contracts/memory.ts";

export interface SemanticSearch {
  search(args: SearchQuery): Promise<SearchResult>;
  status(): Promise<SearchStatus>;
}
export class LocalSearch implements SemanticSearch {
  constructor(readonly connectionFile: string, readonly keyFile: string) {}
  async #call(method: string, args: unknown) {
    try {
      const c = JSON.parse(await Deno.readTextFile(this.connectionFile));
      const url = new URL(c.url);
      if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port) {
        throw new KernelError("rejected", "语义搜索仅连接本机");
      }
      const response = await fetch(`${url.origin}/${method}`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${(await Deno.readTextFile(this.keyFile)).trim()}`,
          "content-type": "application/json",
        },
        body: JSON.stringify(args),
        signal: AbortSignal.timeout(method === "status" ? 1500 : 30000),
        redirect: "error",
      });
      if (!response.ok) {
        await response.body?.cancel();
        throw new KernelError(
          response.status === 400 ? "rejected" : "unavailable",
          "语义搜索暂不可用或查询参数无效",
        );
      }
      return await response.json();
    } catch (error) {
      if (error instanceof KernelError) throw error;
      throw new KernelError("unavailable", "本机语义索引暂不可用，原始记录与字面搜索仍可使用");
    }
  }
  search(args: SearchQuery): Promise<SearchResult> {
    return this.#call("search", args);
  }
  status(): Promise<SearchStatus> {
    return this.#call("status", {});
  }
}
