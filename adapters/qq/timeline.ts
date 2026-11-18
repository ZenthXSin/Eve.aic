// 只保存事实引用和稳定编号；正文与平台回执继续留在入站/发送原件中。
import type { PhoneMessage } from "../../contracts/phone.ts";
import { writeAtomic } from "./journal.ts";

export interface TimelineSource {
  key: string;
  fingerprint: string;
  observedAt: string;
  message: Omit<PhoneMessage, "seq">;
  notify: boolean;
}
export type TimelineFact = Pick<TimelineSource, "key" | "fingerprint" | "observedAt">;
interface Entry {
  seq: number;
  key: string;
  fingerprint: string;
}
export class QqTimelineError extends Error {
  constructor(readonly category: "storage" | "corrupt" | "unavailable") {
    super(`QQ 时间线未完成：${category}，已有事实保留`);
    this.name = "QqTimelineError";
  }
}

export class QqTimeline {
  #entries: Entry[] = [];
  #keys = new Set<string>();
  #verifiedThrough = 0;
  #pending = 0;
  #lock?: Deno.FsFile;
  #failed = false;
  #tail: Promise<unknown> = Promise.resolve();
  #closing = false;

  constructor(readonly directory: string) {}

  get available(): boolean {
    return this.#lock !== undefined && !this.#failed && !this.#closing;
  }

  async initialize(): Promise<void> {
    if (this.#lock || this.#failed || this.#closing) throw new QqTimelineError("unavailable");
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
      for await (const file of Deno.readDir(this.directory)) {
        if (!file.isFile || !["index.json", "writer.lock"].includes(file.name)) {
          throw new QqTimelineError("corrupt");
        }
      }
      lock = await Deno.open(`${this.directory}/writer.lock`, {
        read: true,
        write: true,
        create: true,
        mode: 0o600,
      });
      if (!(await lock.tryLock(true))) throw new QqTimelineError("unavailable");
      const path = `${this.directory}/index.json`;
      try {
        const stat = await Deno.lstat(path);
        if (!stat.isFile || stat.isSymlink || stat.size > 134_217_728) {
          throw new QqTimelineError("corrupt");
        }
        const data = JSON.parse(await Deno.readTextFile(path));
        if (data?.schema !== 1 || !Array.isArray(data.entries)) {
          throw new QqTimelineError("corrupt");
        }
        for (const [index, e] of data.entries.entries()) {
          if (
            e?.seq !== index + 1 || typeof e.key !== "string" ||
            !/^(incoming:[1-9][0-9]*|outgoing:[a-zA-Z0-9_-]{1,128})$/.test(e.key) ||
            !/^[a-f0-9]{64}$/.test(e.fingerprint) || this.#keys.has(e.key)
          ) throw new QqTimelineError("corrupt");
          this.#keys.add(e.key);
          this.#entries.push({ seq: e.seq, key: e.key, fingerprint: e.fingerprint });
        }
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      this.#lock = lock;
    } catch (error) {
      lock?.close();
      this.#failed = true;
      if (error instanceof QqTimelineError) throw error;
      throw new QqTimelineError("corrupt");
    }
  }

  async snapshot(sources: readonly TimelineSource[]) {
    if (!this.#lock || this.#failed || this.#closing) throw new QqTimelineError("unavailable");
    const copy = structuredClone(sources);
    const keys = await this.sync(0, copy);
    const byKey = new Map(copy.map((source) => [source.key, source]));
    return keys.map((key, index) => {
      const source = byKey.get(key)!;
      return { key, message: { ...source.message, seq: index + 1 }, notify: source.notify };
    });
  }

  current(through: number): boolean {
    return this.available && !this.#pending && through === this.#verifiedThrough &&
      through === this.#entries.length;
  }

  async sync(through: number, sources: readonly TimelineFact[]): Promise<string[]> {
    if (!this.available) throw new QqTimelineError("unavailable");
    const copy = sources.map(({ key, fingerprint, observedAt }) => ({
      key,
      fingerprint,
      observedAt,
    }));
    this.#pending++;
    const operation = this.#tail.then(async () => {
      if (this.#failed) throw new QqTimelineError("unavailable");
      const byKey = new Map<string, TimelineFact>();
      try {
        if (!Number.isSafeInteger(through) || through < 0 || through > this.#verifiedThrough) {
          throw new QqTimelineError("corrupt");
        }
        for (const source of copy) {
          if (
            !/^(incoming:[1-9][0-9]*|outgoing:[a-zA-Z0-9_-]{1,128})$/.test(source.key) ||
            !/^[a-f0-9]{64}$/.test(source.fingerprint) || byKey.has(source.key) ||
            !Number.isFinite(Date.parse(source.observedAt))
          ) throw new QqTimelineError("corrupt");
          byKey.set(source.key, source);
        }
        for (let index = through; index < this.#entries.length; index++) {
          const entry = this.#entries[index];
          if (byKey.get(entry.key)?.fingerprint !== entry.fingerprint) {
            throw new QqTimelineError("corrupt");
          }
          byKey.delete(entry.key);
        }
        const fresh = [...byKey.values()].sort((a, b) =>
          Date.parse(a.observedAt) - Date.parse(b.observedAt) || a.key.localeCompare(b.key)
        );
        if (fresh.length) {
          const next = [...this.#entries];
          for (const source of fresh) {
            if (this.#keys.has(source.key)) throw new QqTimelineError("corrupt");
            next.push({ seq: next.length + 1, key: source.key, fingerprint: source.fingerprint });
          }
          try {
            await writeAtomic(`${this.directory}/index.json`, { schema: 1, entries: next });
          } catch {
            throw new QqTimelineError("storage");
          }
          this.#entries = next;
          for (const source of fresh) this.#keys.add(source.key);
        }
        this.#verifiedThrough = this.#entries.length;
        return this.#entries.slice(through).map((entry) => entry.key);
      } catch (error) {
        this.#failed = true;
        throw error;
      }
    }).finally(() => this.#pending--);
    this.#tail = operation.catch(() => {});
    return await operation;
  }

  async close(): Promise<void> {
    this.#closing = true;
    await this.#tail;
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failed) throw new QqTimelineError("storage");
  }
}
