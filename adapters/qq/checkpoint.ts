import { writeAtomic } from "./journal.ts";

export interface GatewaySession {
  sessionId: string;
  sequence: number;
}

export interface GatewayCheckpoint {
  load(): Promise<GatewaySession | undefined>;
  save(session: GatewaySession | undefined): Promise<void>;
}

export class GatewayStorageError extends Error {
  constructor(readonly category: "storage" | "corrupt") {
    super(`QQ 网关水位未完成：${category}，已有记录保留`);
    this.name = "GatewayStorageError";
  }
}

function valid(session: unknown): session is GatewaySession {
  if (session === null || typeof session !== "object" || Array.isArray(session)) return false;
  const record = session as GatewaySession;
  return typeof record.sessionId === "string" && record.sessionId.length > 0 &&
    record.sessionId.length <= 512 &&
    Number.isSafeInteger(record.sequence) && record.sequence >= 0;
}

/** 文件只含恢复登录态和已持久化的消费序号，不保存访问凭据。 */
export class FileGatewayCheckpoint implements GatewayCheckpoint {
  #failed = false;
  constructor(readonly path: string) {}

  async load(): Promise<GatewaySession | undefined> {
    if (this.#failed) throw new GatewayStorageError("storage");
    try {
      const file = await Deno.lstat(this.path);
      if (!file.isFile || file.isSymlink || file.size > 8192) throw new Error();
      const data = JSON.parse(await Deno.readTextFile(this.path));
      if (data.schema !== 1 || (data.session !== null && !valid(data.session))) throw new Error();
      return data.session === null ? undefined : data.session;
    } catch (error) {
      if (error instanceof Deno.errors.NotFound) return undefined;
      this.#failed = true;
      throw new GatewayStorageError("corrupt");
    }
  }

  async save(session: GatewaySession | undefined): Promise<void> {
    if (this.#failed || (session !== undefined && !valid(session))) {
      throw new GatewayStorageError("storage");
    }
    try {
      await Deno.mkdir(this.path.slice(0, this.path.lastIndexOf("/")), {
        recursive: true,
        mode: 0o700,
      });
      await writeAtomic(this.path, { schema: 1, session: session ?? null });
    } catch {
      this.#failed = true;
      throw new GatewayStorageError("storage");
    }
  }
}
