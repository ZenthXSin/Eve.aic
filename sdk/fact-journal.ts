import { Buffer } from "node:buffer";
import { createHash } from "node:crypto";
import { canonical, fingerprint } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";
import { writeDurableJson } from "./durable-json.ts";

export interface FactEvent {
  schema: 1;
  seq: number;
  at: string;
  kind: string;
  data: Json;
  previous: string;
  digest: string;
}
const ZERO = "0".repeat(64);
// An accepted 8 MiB binary image expands to ~11 MiB in JSON/base64.
// Keep a single original below the 16 MiB RPC envelope without cutting its data.
const MAX_RECORD_BYTES = 12_582_912;
const PAGE_BYTES = 8_388_608;
// Rendering a turn needs a few recent passes, not a second 64 MiB history heap.
const READ_CACHE_BYTES = 8_388_608;
const CHUNK_RECORDS = 4096;
const CHECKPOINT_INTERVAL = 1024;
const MAX_CHECKPOINT_BYTES = 32 * 1024 * 1024;
interface FactCheckpointProfile {
  version: string;
  project(event: FactEvent): Json;
}
interface FactCheckpoint {
  schema: 1;
  profile: string;
  through: number;
  digests: string[];
  projection: Json[];
}
function filename(seq: number) {
  return `${String(seq).padStart(16, "0")}.json`;
}
function filesystemCategory(error: unknown): string {
  return Object.entries(Deno.errors).find(([, type]) => error instanceof type)?.[0] ?? "unknown";
}
function indexOf(property: PropertyKey): number | undefined {
  if (typeof property !== "string" || !/^(0|[1-9]\d*)$/.test(property)) return;
  const index = Number(property);
  return Number.isSafeInteger(index) ? index : undefined;
}
const journals = new WeakMap<FactEvent[], FactJournal>();
/** Reuse verified bytes only within a synchronous projection. No historical bodies
 * survive the call; the next operation rechecks disk originals and digest anchors. */
export function withFactReadCache<T>(events: FactEvent[], operation: () => T): T {
  const journal = journals.get(events);
  return journal ? journal.withReadCache(operation) : operation();
}
export function factProjectionAt(events: FactEvent[], index: number): Json | undefined {
  return journals.get(events)?.projectionAt(index);
}
/**
 * 原件只追加。常驻索引每条仅 32 字节摘要，不保留所有历史正文。
 * events 是兼容旧调用的只读、按需读取数组视图；filter/map 等结果仍由调用者持有。
 * 面向长历史的新调用使用 readPage，避免把整份历史另行收集到数组。
 */
export class FactJournal {
  readonly events: FactEvent[];
  #view: FactEvent[] = [];
  #readCache?: { records: Map<number, string>; bytes: number };
  withReadCache<T>(operation: () => T): T {
    if (this.#readCache) return operation();
    this.#readCache = { records: new Map(), bytes: 0 };
    try {
      return operation();
    } finally {
      this.#readCache = undefined;
    }
  }
  #digests: Uint8Array[] = [];
  #count = 0;
  #projection: Json[] = [];
  #checkpointThrough = 0;
  #checkpointAttemptThrough = 0;
  #checkpointWork?: Promise<void>;
  #lock?: Deno.FsFile;
  #failed = false;
  #failure?: KernelError;
  #fail(error: KernelError): KernelError {
    this.#failed = true;
    return this.#failure ??= error;
  }
  #tail: Promise<unknown> = Promise.resolve();
  #closed = false;
  #closing = false;
  #initializing = false;
  #initializationFinished?: Promise<void>;
  constructor(readonly directory: string, readonly checkpoint?: FactCheckpointProfile) {
    this.events = new Proxy(this.#view, {
      get: (target, property, receiver) => {
        if (property === "length") return this.#count;
        const index = indexOf(property);
        return index === undefined
          ? Reflect.get(target, property, receiver)
          : this.#read(index + 1);
      },
      has: (target, property) => {
        const index = indexOf(property);
        return index === undefined ? Reflect.has(target, property) : index < this.#count;
      },
      getOwnPropertyDescriptor: (target, property) => {
        if (property === "length") {
          return { ...Reflect.getOwnPropertyDescriptor(target, property)!, value: this.#count };
        }
        const index = indexOf(property);
        if (index === undefined) return Reflect.getOwnPropertyDescriptor(target, property);
        if (index >= this.#count) return undefined;
        return {
          configurable: true,
          enumerable: true,
          writable: false,
          value: this.#read(index + 1),
        };
      },
      ownKeys: () => [...Array.from({ length: this.#count }, (_, i) => String(i)), "length"],
      set: () => {
        throw new KernelError("rejected", "事实日志视图只读，请通过 append 提交");
      },
      defineProperty: () => {
        throw new KernelError("rejected", "事实日志视图只读");
      },
      deleteProperty: () => {
        throw new KernelError("rejected", "事实日志原件不得删除");
      },
      preventExtensions: () => {
        throw new KernelError("rejected", "事实日志视图不能封闭");
      },
      setPrototypeOf: () => {
        throw new KernelError("rejected", "事实日志视图不能修改原型");
      },
    });
    journals.set(this.events, this);
  }
  get count() {
    return this.#count;
  }
  get available() {
    return this.#lock !== undefined && !this.#failed && !this.#closed;
  }
  projectionAt(index: number): Json | undefined {
    if (this.#failure) throw this.#failure;
    return this.#projection[index];
  }
  #digest(seq: number) {
    if (seq === 0) return ZERO;
    const offset = (seq - 1) % CHUNK_RECORDS * 32;
    return Array.from(
      this.#digests[Math.floor((seq - 1) / CHUNK_RECORDS)].subarray(offset, offset + 32),
      (byte) => byte.toString(16).padStart(2, "0"),
    ).join("");
  }
  #index(event: FactEvent) {
    if (this.checkpoint) this.#projection.push(this.checkpoint.project(event));
    const chunk = Math.floor(this.#count / CHUNK_RECORDS);
    this.#digests[chunk] ??= new Uint8Array(CHUNK_RECORDS * 32);
    const bytes = Uint8Array.from(event.digest.match(/../g)!, (hex) => parseInt(hex, 16));
    this.#digests[chunk].set(bytes, this.#count % CHUNK_RECORDS * 32);
    this.#count++;
  }
  async #readOriginal(seq: number) {
    const path = `${this.directory}/events/${filename(seq)}`;
    const stat = await Deno.lstat(path);
    if (!stat.isFile || stat.isSymlink || stat.size > MAX_RECORD_BYTES) {
      throw new Error("invalid file");
    }
    return await Deno.readTextFile(path);
  }
  async #restoreCheckpoint(count: number) {
    const path = `${this.directory}/index-checkpoint.json`;
    try {
      const stat = await Deno.lstat(path);
      if (!stat.isFile || stat.isSymlink || stat.size > MAX_CHECKPOINT_BYTES) return;
      const { state, checksum } = JSON.parse(await Deno.readTextFile(path)) as {
        state: FactCheckpoint;
        checksum: string;
      };
      if (
        state?.schema !== 1 || state.profile !== this.checkpoint?.version ||
        !Number.isSafeInteger(state.through) || state.through < 1 || state.through > count ||
        !Array.isArray(state.digests) ||
        state.digests.length !== Math.ceil(state.through / CHUNK_RECORDS) ||
        !Array.isArray(state.projection) || state.projection.length !== state.through ||
        checksum !== createHash("sha256").update(JSON.stringify(state)).digest("hex")
      ) return;
      const digests = state.digests.map((encoded, index) => {
        const length = Math.min(CHUNK_RECORDS, state.through - index * CHUNK_RECORDS) * 32;
        if (typeof encoded !== "string" || encoded.length !== Math.ceil(length / 3) * 4) {
          throw new Error("invalid checkpoint digest length");
        }
        const decoded = Buffer.from(encoded, "base64");
        if (decoded.length !== length) throw new Error("invalid checkpoint digest chunk");
        const chunk = new Uint8Array(CHUNK_RECORDS * 32);
        chunk.set(decoded);
        return chunk;
      });
      this.#digests = digests;
      const anchor = this.#validate(
        await this.#readOriginal(state.through),
        state.through,
        this.#digest(state.through - 1),
      );
      if (anchor.digest !== this.#digest(state.through)) throw new Error("checkpoint anchor");
      this.#projection = state.projection;
      this.#count = this.#checkpointThrough = state.through;
    } catch {
      this.#digests = [];
    }
  }
  async #writeCheckpoint() {
    if (!this.checkpoint || !this.available || this.#count === this.#checkpointThrough) return;
    const through = this.#count;
    this.#checkpointAttemptThrough = through;
    const path = `${this.directory}/index-checkpoint.json`;
    const temporary = `${path}.tmp`;
    try {
      const state: FactCheckpoint = {
        schema: 1,
        profile: this.checkpoint.version,
        through,
        digests: this.#digests.map((chunk, index) =>
          Buffer.from(chunk.subarray(0, Math.min(CHUNK_RECORDS, through - index * CHUNK_RECORDS) * 32))
            .toString("base64")
        ),
        projection: this.#projection,
      };
      const contents = JSON.stringify(state);
      const checksum = createHash("sha256").update(contents).digest("hex");
      const bytes = new TextEncoder().encode(`{"state":${contents},"checksum":"${checksum}"}\n`);
      if (bytes.length > MAX_CHECKPOINT_BYTES) return;
      await Deno.remove(temporary).catch(() => {});
      await Deno.writeFile(temporary, bytes, { createNew: true, mode: 0o600 });
      await Deno.rename(temporary, path);
      this.#checkpointThrough = through;
    } catch {
      await Deno.remove(temporary).catch(() => {});
    }
  }
  #validate(raw: string, seq: number, previous: string): FactEvent {
    const event: FactEvent = JSON.parse(raw);
    if (!event || typeof event !== "object" || Array.isArray(event)) {
      throw new Error("invalid event");
    }
    const { digest, ...body } = event;
    if (
      event.schema !== 1 || event.seq !== seq || event.previous !== previous ||
      typeof event.at !== "string" || !Number.isFinite(Date.parse(event.at)) ||
      typeof event.kind !== "string" || !event.kind || !/^[a-f0-9]{64}$/.test(digest) ||
      digest !== createHash("sha256").update(canonical(body as unknown as Json)).digest("hex")
    ) throw new Error("invalid chain");
    return event;
  }
  #read(seq: number): FactEvent | undefined {
    if (seq < 1 || seq > this.#count) return;
    if (this.#failed) throw new KernelError("corrupt", "事实日志已失效，原件保留");
    const cached = this.#readCache?.records.get(seq);
    if (cached !== undefined) return JSON.parse(cached);
    try {
      const path = `${this.directory}/events/${filename(seq)}`;
      const stat = Deno.lstatSync(path);
      if (!stat.isFile || stat.isSymlink || stat.size > MAX_RECORD_BYTES) {
        throw new Error("invalid file");
      }
      const raw = Deno.readTextFileSync(path);
      const event = this.#validate(raw, seq, this.#digest(seq - 1));
      if (event.digest !== this.#digest(seq)) throw new Error("changed original");
      const cache = this.#readCache;
      // Account UTF-16 storage conservatively, without retaining unbounded history.
      if (cache && cache.bytes + raw.length * 2 <= READ_CACHE_BYTES) {
        cache.records.set(seq, raw);
        cache.bytes += raw.length * 2;
      }
      return event;
    } catch (error) {
      throw this.#fail(
        new KernelError(
          "corrupt",
          `事实日志原件变化或读取失败（${filesystemCategory(error)}），原件保留`,
        ),
      );
    }
  }
  /** 每页目标 8 MiB；单份较大原件独占一页，最大 12 MiB，保持原件完整。 */
  readPage(afterSeq = 0, limit = 8, throughSeq = this.#count) {
    if (
      !Number.isSafeInteger(afterSeq) || afterSeq < 0 ||
      !Number.isSafeInteger(limit) || limit < 1 || limit > 128 ||
      !Number.isSafeInteger(throughSeq) || throughSeq < afterSeq || throughSeq > this.#count
    ) {
      throw new KernelError("rejected", "事实日志分页游标无效");
    }
    const events: FactEvent[] = [];
    let bytes = 0;
    for (let seq = afterSeq + 1; seq <= Math.min(throughSeq, afterSeq + limit); seq++) {
      const event = this.#read(seq)!;
      const size = new TextEncoder().encode(JSON.stringify(event)).length;
      if (events.length && bytes + size > PAGE_BYTES) break;
      events.push(event);
      bytes += size;
    }
    const nextSeq = afterSeq + events.length;
    return { events, throughSeq, nextSeq, hasMore: nextSeq < throughSeq };
  }
  #recovery = { phase: "pending", read: 0, total: 0 };
  get recovery() {
    return { ...this.#recovery };
  }
  /** Consume each verified original once; callbacks must be synchronous. */
  async initialize(replay?: (event: FactEvent) => void) {
    if (this.#lock || this.#closed || this.#closing || this.#initializing || this.#failed) {
      throw new KernelError("unavailable", "事实日志已被持有或不可恢复");
    }
    this.#initializing = true;
    let finished!: () => void;
    this.#initializationFinished = new Promise<void>((resolve) => {
      finished = resolve;
    });
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(`${this.directory}/events`, { recursive: true, mode: 0o700 });
      for (const path of [this.directory, `${this.directory}/events`]) {
        const stat = await Deno.lstat(path);
        if (!stat.isDirectory || stat.isSymlink) throw new Error("invalid directory");
      }
      const lockPath = `${this.directory}/writer.lock`;
      try {
        const stat = await Deno.lstat(lockPath);
        if (!stat.isFile || stat.isSymlink) throw new Error("invalid lock");
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      lock = await Deno.open(lockPath, { create: true, read: true, write: true, mode: 0o600 });
      if (!(await lock.tryLock(true))) {
        throw new KernelError("unavailable", "已有写入者持有事实日志");
      }
      this.#lock = lock;
      this.#recovery.phase = "counting";
      let count = 0;
      let maximum = 0;
      for await (const entry of Deno.readDir(`${this.directory}/events`)) {
        if (!entry.isFile || !/^\d{16}\.json$/.test(entry.name)) {
          throw new KernelError("corrupt", "事实日志有未完成或未知记录，原件保留");
        }
        const sequence = Number(entry.name.slice(0, -5));
        if (!Number.isSafeInteger(sequence) || sequence < 1) {
          throw new KernelError("corrupt", "事实日志序号无效，原件保留");
        }
        maximum = Math.max(maximum, sequence);
        count++;
      }
      if (maximum !== count) throw new KernelError("corrupt", "事实日志存在缺失原件");
      if (this.checkpoint && !replay && count) await this.#restoreCheckpoint(count);
      this.#recovery = { phase: "reading", read: this.#count, total: count };
      // Bounded prefetch overlaps filesystem waits. Validate and project in chain order;
      // never retain the complete history or release the lock with unfinished reads.
      for (let start = this.#count + 1; start <= count; start += 4) {
        const reads = await Promise.allSettled(
          Array.from({ length: Math.min(4, count - start + 1) }, (_, offset) =>
            this.#readOriginal(start + offset)
          ),
        );
        for (let offset = 0; offset < reads.length; offset++) {
          const read = reads[offset];
          if (read.status === "rejected") throw read.reason;
          const seq = start + offset;
          const event = this.#validate(read.value, seq, this.#digest(seq - 1));
          this.#index(event);
          replay?.(event);
          this.#recovery.read = seq;
        }
        // Keep status, shutdown and other services responsive during long recovery.
        if ((start - 1) % 128 < 4) await new Promise((resolve) => setTimeout(resolve, 0));
      }
      await this.#writeCheckpoint();
      this.#recovery.phase = "ready";
    } catch (error) {
      this.#recovery.phase = "failed";
      lock?.close();
      this.#lock = undefined;
      throw this.#fail(
        error instanceof KernelError
          ? error
          : new KernelError("corrupt", "事实日志读取失败或来源链不连续，原件保留"),
      );
    } finally {
      this.#initializing = false;
      finished();
    }
  }
  async append(kind: string, data: Json): Promise<FactEvent> {
    if (this.#closing || this.#closed) throw new KernelError("storage", "事实日志已关闭");
    if (typeof kind !== "string" || !kind) {
      throw new KernelError("rejected", "事实记录类型不能为空");
    }
    const snapshot = structuredClone(data);
    const operation = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "事实日志不可提交");
      const body = {
        schema: 1 as const,
        seq: this.#count + 1,
        at: new Date().toISOString(),
        kind,
        data: snapshot,
        previous: this.#digest(this.#count),
      };
      const event = { ...body, digest: await fingerprint(body as unknown as Json) };
      if (new TextEncoder().encode(JSON.stringify(event)).length + 1 > MAX_RECORD_BYTES) {
        throw new KernelError("storage", "事实记录超出单份记录格式，原件保留");
      }
      return await this.#store(event);
    });
    this.#tail = operation.catch(() => {});
    return await operation;
  }
  /** 导入稳定出口已提交的原件，保留原始序号、时间及摘要；重复同一原件幂等。 */
  async importOriginal(original: FactEvent): Promise<FactEvent> {
    if (this.#closing || this.#closed) throw new KernelError("storage", "事实日志已关闭");
    const snapshot = structuredClone(original);
    const operation = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "事实日志不可提交");
      let event: FactEvent;
      try {
        if (!Number.isSafeInteger(snapshot.seq) || snapshot.seq < 1) throw new Error("invalid seq");
        const raw = JSON.stringify(snapshot);
        if (new TextEncoder().encode(raw).length + 1 > MAX_RECORD_BYTES) {
          throw new Error("oversize original");
        }
        if (snapshot.seq <= this.#count) {
          const saved = this.#read(snapshot.seq)!;
          if (canonical(saved as unknown as Json) !== canonical(snapshot as unknown as Json)) {
            throw new Error("conflicting original");
          }
          return saved;
        }
        event = this.#validate(raw, this.#count + 1, this.#digest(this.#count));
      } catch {
        throw this.#fail(new KernelError("corrupt", "导入原件与已有事实链不符，原件保留"));
      }
      return await this.#store(event);
    });
    this.#tail = operation.catch(() => {});
    return await operation;
  }
  async #store(event: FactEvent): Promise<FactEvent> {
    try {
      const path = `${this.directory}/events/${filename(event.seq)}`;
      try {
        await Deno.lstat(path);
        throw new Error("事实原件已经存在，禁止覆盖");
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      await writeDurableJson(path, event);
    } catch (error) {
      // Report only a known filesystem category, never arbitrary exception text or paths.
      const category = filesystemCategory(error);
      throw this.#fail(new KernelError("storage", `事实日志存盘失败（${category}），保留原件`));
    }
    this.#index(event);
    if (
      this.checkpoint && !this.#checkpointWork &&
      this.#count - Math.max(this.#checkpointThrough, this.#checkpointAttemptThrough) >=
        CHECKPOINT_INTERVAL
    ) {
      this.#checkpointWork = this.#writeCheckpoint().finally(() => {
        this.#checkpointWork = undefined;
      });
    }
    return structuredClone(event);
  }
  async close() {
    this.#closing = true;
    await this.#initializationFinished;
    await this.#tail;
    await this.#checkpointWork;
    await this.#writeCheckpoint();
    this.#closed = true;
    this.#lock?.close();
    this.#lock = undefined;
    if (this.#failure) {
      const error = new KernelError(
        "storage",
        `事实日志关闭时保留首因：${this.#failure.kind}: ${this.#failure.message}`,
      );
      error.cause = this.#failure;
      throw error;
    }
  }
}
