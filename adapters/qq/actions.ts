import { QqClient, type QqSendResult, type QqSendText, validateQqSendText } from "./client.ts";
import { writeAtomic } from "./journal.ts";

export interface SendRecord {
  schema: 1;
  actionId: string;
  fingerprint: string;
  request: QqSendText;
  startedAt: string;
  status: "started" | "finished";
  finishedAt?: string;
  result?: QqSendResult;
}

export type SendStatus =
  | { actionId: string; status: "running" }
  | { actionId: string; status: "unknown"; reason: "interrupted" }
  | { actionId: string; status: "finished"; result: QqSendResult };

export class QqActionError extends Error {
  constructor(readonly category: "request" | "conflict" | "storage" | "corrupt" | "unavailable") {
    super(`QQ 动作未完成：${category}，已有回执保留`);
    this.name = "QqActionError";
  }
}

function canonical(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const record = value as Record<string, unknown>;
  return `{${
    Object.keys(record).filter((k) => record[k] !== undefined).sort().map((k) =>
      `${JSON.stringify(k)}:${canonical(record[k])}`
    ).join(",")
  }}`;
}

async function fingerprint(request: QqSendText): Promise<string> {
  const hash = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(request)));
  return Array.from(new Uint8Array(hash), (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function validateId(id: string): void {
  if (typeof id !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(id)) {
    throw new QqActionError("request");
  }
}

function validResult(value: unknown): value is QqSendResult {
  if (value === null || typeof value !== "object") return false;
  const r = value as QqSendResult;
  if (r.status === "sent") return typeof r.messageId === "string" && r.messageId.length > 0;
  if (
    !((r.status === "failed" || r.status === "unknown") &&
      typeof r.attempted === "boolean" && (r.status !== "unknown" || r.attempted) &&
      typeof r.error === "object" && r.error !== null)
  ) return false;
  if (
    ![
      "scope",
      "request",
      "authentication",
      "rate_limit",
      "platform",
      "transport",
      "invalid_response",
      "pending",
      "duplicate",
    ].includes(r.error.category)
  ) return false;
  for (const field of [r.error.httpStatus, r.error.code]) {
    if (field !== undefined && (!Number.isSafeInteger(field) || field < 0)) return false;
  }
  return r.error.traceId === undefined ||
    (typeof r.error.traceId === "string" && /^[a-zA-Z0-9_.:-]{1,256}$/.test(r.error.traceId));
}

/** 一份发送账本只有一个持有者；每个真实动作在网络前写 started，实际回执写 finished。 */
export class QqSendActions {
  #records = new Map<string, SendRecord>();
  #inflight = new Map<string, Promise<SendStatus>>();
  #lock?: Deno.FsFile;
  #failed = false;
  #closing = false;

  constructor(readonly client: QqClient, readonly directory: string) {}

  async initialize(): Promise<void> {
    if (this.#lock || this.#failed || this.#closing) throw new QqActionError("unavailable");
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(`${this.directory}/records`, { recursive: true, mode: 0o700 });
      lock = await Deno.open(`${this.directory}/writer.lock`, {
        read: true,
        write: true,
        create: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) throw new QqActionError("unavailable");
      for await (const entry of Deno.readDir(`${this.directory}/records`)) {
        if (!entry.isFile || !/^[a-zA-Z0-9_-]{1,128}\.json$/.test(entry.name)) {
          throw new QqActionError("corrupt");
        }
        const path = `${this.directory}/records/${entry.name}`;
        const stat = await Deno.lstat(path);
        if (!stat.isFile || stat.isSymlink || stat.size > 2_097_152) {
          throw new QqActionError("corrupt");
        }
        const record: SendRecord = JSON.parse(await Deno.readTextFile(path));
        validateId(record.actionId);
        validateQqSendText(record.request);
        if (
          record.schema !== 1 || entry.name !== `${record.actionId}.json` ||
          record.fingerprint !== await fingerprint(record.request) ||
          this.#records.has(record.actionId) ||
          !Number.isFinite(Date.parse(record.startedAt)) ||
          !["started", "finished"].includes(record.status) ||
          (record.status === "finished" &&
            (!Number.isFinite(Date.parse(record.finishedAt ?? "")) ||
              !validResult(record.result))) ||
          (record.status === "started" &&
            (record.result !== undefined || record.finishedAt !== undefined))
        ) throw new QqActionError("corrupt");
        this.#records.set(record.actionId, record);
      }
      this.#lock = lock;
    } catch (error) {
      lock?.close();
      this.#failed = true;
      if (error instanceof QqActionError) throw error;
      throw new QqActionError("corrupt");
    }
  }

  #available(): void {
    if (!this.#lock || this.#closing || this.#failed) throw new QqActionError("unavailable");
  }

  status(actionId: string): SendStatus | undefined {
    this.#available();
    validateId(actionId);
    const record = this.#records.get(actionId);
    if (!record) return this.#inflight.has(actionId) ? { actionId, status: "running" } : undefined;
    if (record.status === "finished") {
      return { actionId, status: "finished", result: structuredClone(record.result!) };
    }
    return this.#inflight.has(actionId)
      ? { actionId, status: "running" }
      : { actionId, status: "unknown", reason: "interrupted" };
  }

  async #save(record: SendRecord): Promise<void> {
    try {
      await writeAtomic(`${this.directory}/records/${record.actionId}.json`, record);
    } catch {
      this.#failed = true;
      throw new QqActionError("storage");
    }
    this.#records.set(record.actionId, record);
  }

  async send(actionId: string, request: QqSendText): Promise<SendStatus> {
    this.#available();
    validateId(actionId);
    const snapshot = structuredClone(request);
    try {
      validateQqSendText(snapshot);
    } catch {
      throw new QqActionError("request");
    }
    if (new TextEncoder().encode(JSON.stringify(snapshot)).length > 1_048_576) {
      throw new QqActionError("request");
    }
    // 在任何 await 之前登记本动作 Promise；同键并发不能各自开始网络发送。
    const existing = this.#inflight.get(actionId);
    if (existing) {
      await existing;
      return await this.send(actionId, snapshot);
    }
    const operation = (async (): Promise<SendStatus> => {
      const hash = await fingerprint(snapshot);
      const previous = this.#records.get(actionId);
      if (previous) {
        if (previous.fingerprint !== hash) throw new QqActionError("conflict");
        return previous.status === "finished"
          ? { actionId, status: "finished", result: structuredClone(previous.result!) }
          : { actionId, status: "unknown", reason: "interrupted" };
      }
      const started: SendRecord = {
        schema: 1,
        actionId,
        fingerprint: hash,
        request: snapshot,
        startedAt: new Date().toISOString(),
        status: "started",
      };
      await this.#save(started);
      if (this.#failed) throw new QqActionError("storage");
      // private 也在动作层落实，不能因传入了通用客户端而发群消息。
      const received: QqSendResult = snapshot.destination.kind === "user"
        ? await this.client.sendText(snapshot)
        : { status: "failed", attempted: false, error: { category: "scope" } };
      // 回执通过 JSON 边界返回；首次与重启后保持同一份结构，不夹带 undefined 字段。
      const result: QqSendResult = JSON.parse(JSON.stringify(received));
      await this.#save({
        ...started,
        status: "finished",
        finishedAt: new Date().toISOString(),
        result,
      });
      return { actionId, status: "finished", result: structuredClone(result) };
    })();
    this.#inflight.set(actionId, operation);
    try {
      return await operation;
    } finally {
      this.#inflight.delete(actionId);
    }
  }

  /** 等待已经开始的实际动作保存后再释放目录锁，避免新实例与迟到旧动作并写。 */
  async close(): Promise<void> {
    this.#closing = true;
    const results = await Promise.allSettled([...this.#inflight.values()]);
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed || results.some((r) => r.status === "rejected")) {
      throw new QqActionError("storage");
    }
  }
}
