// 独立适配器的只追加入站日志。先完整持久化，再允许网关提交消费水位。
import { type InboundQqMessage, type QqDispatch, qqMessage } from "./inbound.ts";

export interface InboundRecord {
  schema: 1;
  seq: number;
  receivedAt: string;
  identity: string;
  message: InboundQqMessage;
  raw: QqDispatch;
}

export class QqJournalError extends Error {
  constructor(readonly category: "storage" | "corrupt" | "conflict") {
    super(`QQ 日志未完成：${category}，原有数据保留`);
    this.name = "QqJournalError";
  }
}

function canonical(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const object = value as Record<string, unknown>;
  return `{${
    Object.keys(object).sort().map((key) => `${JSON.stringify(key)}:${canonical(object[key])}`)
      .join(",")
  }}`;
}

async function digest(value: unknown): Promise<string> {
  const bytes = new TextEncoder().encode(canonical(value));
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(hash), (b) => b.toString(16).padStart(2, "0")).join("");
}

async function syncDirectory(path: string): Promise<void> {
  const handle = await Deno.open(path, { read: true });
  try {
    await handle.sync();
  } finally {
    handle.close();
  }
}

async function writeAtomic(path: string, data: unknown): Promise<void> {
  const temporary = `${path}.${crypto.randomUUID()}.tmp`;
  const file = await Deno.open(temporary, { createNew: true, write: true, mode: 0o600 });
  try {
    const bytes = new TextEncoder().encode(JSON.stringify(data) + "\n");
    let offset = 0;
    while (offset < bytes.length) {
      const written = await file.write(bytes.subarray(offset));
      if (written === 0) throw new QqJournalError("storage");
      offset += written;
    }
    await file.sync();
  } finally {
    file.close();
  }
  await Deno.rename(temporary, path);
  await syncDirectory(path.slice(0, path.lastIndexOf("/")));
}

/** 一个进程持有日志目录；正式部署还须独立守护/所有权令牌约束多进程写入。 */
export class QqInboundJournal {
  #records = new Map<string, InboundRecord>();
  #seq = 0;
  #tail: Promise<unknown> = Promise.resolve();
  #failed = false;
  #initialized = false;

  constructor(readonly directory: string) {}

  async initialize(): Promise<void> {
    if (this.#initialized) throw new QqJournalError("storage");
    try {
      await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
      const names: string[] = [];
      for await (const entry of Deno.readDir(this.directory)) {
        if (!entry.isFile || !/^\d{16}-[a-f0-9]{64}\.json$/.test(entry.name)) {
          // 未完成写入或外来文件不能静默丢掉；保留并报错，防止恢复时猜测缺口。
          throw new QqJournalError("corrupt");
        }
        names.push(entry.name);
      }
      names.sort();
      for (const name of names) {
        const path = `${this.directory}/${name}`;
        const file = await Deno.lstat(path);
        if (file.isSymlink || file.size > 2_097_152) throw new QqJournalError("corrupt");
        const record: InboundRecord = JSON.parse(await Deno.readTextFile(path));
        if (
          record.schema !== 1 || record.seq !== this.#seq + 1 ||
          name !== `${String(record.seq).padStart(16, "0")}-${record.identity}.json` ||
          await digest(record.message.deduplication) !== record.identity ||
          this.#records.has(record.identity) || typeof record.receivedAt !== "string" ||
          !Number.isFinite(Date.parse(record.receivedAt))
        ) throw new QqJournalError("corrupt");
        if (canonical(qqMessage(record.raw)) !== canonical(record.message)) {
          throw new QqJournalError("corrupt");
        }
        this.#records.set(record.identity, record);
        this.#seq = record.seq;
      }
      this.#initialized = true;
    } catch (error) {
      this.#failed = true;
      if (error instanceof QqJournalError) throw error;
      throw new QqJournalError("corrupt");
    }
  }

  async append(
    message: InboundQqMessage,
    raw: QqDispatch,
  ): Promise<{ record: InboundRecord; fresh: boolean }> {
    // 在调用时复制输入，排队期间调用方修改不能改变已经开始保存的事实。
    const snapshot = structuredClone({ message, raw });
    const operation = this.#tail.then(async () => {
      if (!this.#initialized || this.#failed) throw new QqJournalError("storage");
      if (canonical(qqMessage(snapshot.raw)) !== canonical(snapshot.message)) {
        throw new QqJournalError("conflict");
      }
      const identity = await digest(snapshot.message.deduplication);
      const previous = this.#records.get(identity);
      if (previous) {
        if (canonical(previous.message) !== canonical(snapshot.message)) {
          // 同一身份正文不同不能选择性相信一份，阻止消费水位前进并保留原件。
          this.#failed = true;
          throw new QqJournalError("conflict");
        }
        return { record: structuredClone(previous), fresh: false };
      }
      const record: InboundRecord = {
        schema: 1,
        seq: this.#seq + 1,
        receivedAt: new Date().toISOString(),
        identity,
        ...snapshot,
      };
      if (new TextEncoder().encode(JSON.stringify(record)).length > 2_097_152) {
        throw new QqJournalError("storage");
      }
      try {
        await writeAtomic(
          `${this.directory}/${String(record.seq).padStart(16, "0")}-${identity}.json`,
          record,
        );
      } catch {
        this.#failed = true;
        throw new QqJournalError("storage");
      }
      this.#seq = record.seq;
      this.#records.set(identity, record);
      return { record: structuredClone(record), fresh: true };
    });
    this.#tail = operation.catch(() => {});
    return await operation;
  }

  records(): InboundRecord[] {
    if (!this.#initialized || this.#failed) throw new QqJournalError("storage");
    return structuredClone([...this.#records.values()]);
  }
}
