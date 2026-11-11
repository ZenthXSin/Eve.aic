import { fingerprint } from "../../kernel/calls.ts";
import { type Json, KernelError } from "../../kernel/protocol.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

export interface ConsciousEvent {
  schema: 1;
  seq: number;
  at: string;
  kind: string;
  data: Json;
  previous: string;
  digest: string;
}
export class ConsciousJournal {
  events: ConsciousEvent[] = [];
  #lock?: Deno.FsFile;
  #failed = false;
  #tail: Promise<unknown> = Promise.resolve();
  #closed = false;
  constructor(readonly directory: string) {}
  get available() {
    return this.#lock !== undefined && !this.#failed && !this.#closed;
  }
  async initialize() {
    if (this.#lock || this.#closed) throw new KernelError("unavailable", "意识流已被持有");
    await Deno.mkdir(`${this.directory}/events`, { recursive: true, mode: 0o700 });
    const lock = await Deno.open(`${this.directory}/writer.lock`, {
      create: true,
      read: true,
      write: true,
      mode: 0o600,
    });
    if (!(await lock.tryLock(true))) {
      lock.close();
      throw new KernelError("unavailable", "已有一个脑子持有意识流");
    }
    this.#lock = lock;
    try {
      const names: string[] = [];
      for await (const entry of Deno.readDir(`${this.directory}/events`)) {
        if (!entry.isFile || !/^\d{16}\.json$/.test(entry.name)) {
          throw new KernelError("corrupt", "意识流有未完成或未知记录，原件保留");
        }
        names.push(entry.name);
      }
      names.sort();
      let previous = "0".repeat(64);
      for (const name of names) {
        const path = `${this.directory}/events/${name}`;
        const stat = await Deno.lstat(path);
        if (!stat.isFile || stat.isSymlink || stat.size > 8_388_608) {
          throw new KernelError("corrupt", "意识流原件损坏或过大");
        }
        const event: ConsciousEvent = JSON.parse(await Deno.readTextFile(path));
        const { digest, ...body } = event;
        if (
          event.schema !== 1 || event.seq !== this.events.length + 1 ||
          name !== `${String(event.seq).padStart(16, "0")}.json` || event.previous !== previous ||
          !Number.isFinite(Date.parse(event.at)) || typeof event.kind !== "string" || !event.kind ||
          digest !== await fingerprint(body as unknown as Json)
        ) throw new KernelError("corrupt", "意识流来源链不连续，原件保留");
        this.events.push(event);
        previous = digest;
      }
    } catch (error) {
      this.#failed = true;
      lock.close();
      this.#lock = undefined;
      throw error instanceof KernelError
        ? error
        : new KernelError("corrupt", "意识流读取失败，原件保留");
    }
  }
  async append(kind: string, data: Json): Promise<ConsciousEvent> {
    const snapshot = structuredClone(data);
    const operation = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "意识流不可提交");
      const body = {
        schema: 1 as const,
        seq: this.events.length + 1,
        at: new Date().toISOString(),
        kind,
        data: snapshot,
        previous: this.events.at(-1)?.digest ?? "0".repeat(64),
      };
      const event = { ...body, digest: await fingerprint(body as unknown as Json) };
      if (new TextEncoder().encode(JSON.stringify(event)).length > 8_388_608) {
        throw new KernelError("storage", "意识事件超出单份记录格式，原件保留");
      }
      try {
        await writeDurableJson(
          `${this.directory}/events/${String(event.seq).padStart(16, "0")}.json`,
          event,
        );
      } catch {
        this.#failed = true;
        throw new KernelError("storage", "意识流存盘失败，保留原件");
      }
      this.events.push(event);
      return structuredClone(event);
    });
    this.#tail = operation.catch(() => {});
    return await operation;
  }
  async close() {
    await this.#tail;
    this.#closed = true;
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed) throw new KernelError("storage", "意识流关闭时有存盘失败");
  }
}
