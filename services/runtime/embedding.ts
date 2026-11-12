import { createHash } from "node:crypto";
import { KernelError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

export class LocalSemantic {
  constructor(
    readonly connectionFile: string,
    readonly keyFile: string,
    readonly directory: string,
  ) {}
  async #call(method: string, args: unknown) {
    const connection = JSON.parse(await Deno.readTextFile(this.connectionFile));
    const url = new URL(connection.url);
    if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || !url.port) {
      throw new KernelError("rejected", "语义服务仅连接本机");
    }
    const response = await fetch(`${url.origin}/${method}`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${(await Deno.readTextFile(this.keyFile)).trim()}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(args),
      signal: AbortSignal.timeout(30000),
      redirect: "error",
    });
    if (!response.ok) {
      await response.body?.cancel();
      throw new KernelError("unavailable", "本机语义服务不可用");
    }
    return { connection, value: await response.json() };
  }
  async embed(text: string, purpose: "query" | "passage" = "passage"): Promise<number[]> {
    const connection = JSON.parse(await Deno.readTextFile(this.connectionFile));
    const hash = createHash("sha256").update(
      JSON.stringify([connection.model, connection.revision, purpose, text]),
    ).digest("hex");
    await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
    const path = `${this.directory}/${hash}.json`;
    try {
      const data = JSON.parse(await Deno.readTextFile(path));
      if (
        data.model !== connection.model || data.revision !== connection.revision ||
        !this.#valid(data.vector)
      ) throw new Error();
      return data.vector;
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) {
        throw new KernelError("corrupt", "语义向量索引损坏，保留原件");
      }
    }
    const { value } = await this.#call("embed", { text, purpose });
    if (
      value.model !== connection.model || value.revision !== connection.revision ||
      !this.#valid(value.vector)
    ) throw new KernelError("rejected", "语义模型返回无效或版本改变");
    await writeDurableJson(path, value);
    return value.vector;
  }
  #valid(vector: unknown): vector is number[] {
    return Array.isArray(vector) && vector.length === 384 &&
      vector.every((x) => typeof x === "number" && Number.isFinite(x)) &&
      Math.abs(vector.reduce((sum, x) => sum + x * x, 0) - 1) < 0.01;
  }
  async count(text: string) {
    const { value } = await this.#call("count", { text });
    if (
      !Number.isSafeInteger(value.tokens) || value.tokens < 0 || value.encoding !== "o200k_base"
    ) throw new KernelError("rejected", "分词服务返回无效");
    return value as { tokens: number; encoding: string; modelMappingVerified: false };
  }
}
