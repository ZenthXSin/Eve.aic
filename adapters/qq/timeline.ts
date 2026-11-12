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
        const keys = new Set<string>();
        for (const [index, e] of data.entries.entries()) {
          if (
            e?.seq !== index + 1 || typeof e.key !== "string" ||
            !/^(incoming:[1-9][0-9]*|outgoing:[a-zA-Z0-9_-]{1,128})$/.test(e.key) ||
            !/^[a-f0-9]{64}$/.test(e.fingerprint) || keys.has(e.key)
          ) throw new QqTimelineError("corrupt");
          keys.add(e.key);
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
    const operation = this.#tail.then(async () => {
      if (this.#failed) throw new QqTimelineError("unavailable");
      const byKey = new Map<string, TimelineSource>();
      try {
        for (const source of copy) {
          if (
            !/^(incoming:[1-9][0-9]*|outgoing:[a-zA-Z0-9_-]{1,128})$/.test(source.key) ||
            !/^[a-f0-9]{64}$/.test(source.fingerprint) || byKey.has(source.key) ||
            !Number.isFinite(Date.parse(source.observedAt))
          ) throw new QqTimelineError("corrupt");
          byKey.set(source.key, source);
        }
        const known = new Set<string>();
        for (const entry of this.#entries) {
          if (byKey.get(entry.key)?.fingerprint !== entry.fingerprint) {
            throw new QqTimelineError("corrupt");
          }
          known.add(entry.key);
        }
        const fresh = copy.filter((source) => !known.has(source.key)).sort((a, b) =>
          Date.parse(a.observedAt) - Date.parse(b.observedAt) || a.key.localeCompare(b.key)
        );
        if (fresh.length) {
          const next = [...this.#entries];
          for (const source of fresh) {
            next.push({ seq: next.length + 1, key: source.key, fingerprint: source.fingerprint });
          }
          try {
            await writeAtomic(`${this.directory}/index.json`, { schema: 1, entries: next });
          } catch {
            throw new QqTimelineError("storage");
          }
          this.#entries = next;
        }
        return this.#entries.map((entry) => {
          const source = byKey.get(entry.key)!;
          return {
            key: entry.key,
            message: { ...source.message, seq: entry.seq },
            notify: source.notify,
          };
        });
      } catch (error) {
        this.#failed = true;
        throw error;
      }
    });
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
