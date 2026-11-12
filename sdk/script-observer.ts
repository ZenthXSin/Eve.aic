import { createHash } from "node:crypto";
import { canonical } from "../kernel/calls.ts";
import { type Json, KernelError } from "../kernel/protocol.ts";

interface Ref {
  hash: string;
  bytes: number;
}
const object = (v: unknown): Record<string, unknown> =>
  v && typeof v === "object" && !Array.isArray(v) ? v as Record<string, unknown> : {};
const hash = (v: unknown) => createHash("sha256").update(canonical(v as Json)).digest("hex");
function ref(v: unknown): v is Ref {
  const r = object(v);
  return typeof r.hash === "string" && /^[a-f0-9]{64}$/.test(r.hash) &&
    Number.isSafeInteger(r.bytes) && Number(r.bytes) > 0 && Number(r.bytes) <= 67_108_864;
}
export interface ObservedAction {
  id: string;
  jobId: string;
  namespace: string;
  method: string;
  status: string;
  at: string;
  startedAt: string;
  seq: number;
  sourceSeq: number;
  args: Json;
  result?: Json;
  error?: Json;
  partial: boolean;
}

/** Bounded projection of supplied values; never serializes script source or connection config. */
interface ViewBudget {
  remaining: number;
  partial: boolean;
}
export function observedValue(
  v: unknown,
  depth = 0,
  budget: ViewBudget = { remaining: 8192, partial: false },
): Json {
  if (budget.remaining <= 0) {
    budget.partial = true;
    return "[窗口未展开]";
  }
  if (v === null || typeof v === "boolean" || typeof v === "number") {
    budget.remaining -= 8;
    return v as Json;
  }
  if (typeof v === "string") {
    const text = v.replace(
      /((?:authorization|api[_-]?key|app[_-]?secret|password|private[_-]?key|(?:access[_-]?|refresh[_-]?)?token)["']?\s*[:=]\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*')/gi,
      '$1"[凭据字段不展示]"',
    )
      .replace(/Bearer\s+[A-Za-z0-9._~+/-]+=*/g, "Bearer [凭据字段不展示]");
    const limit = Math.min(4000, budget.remaining);
    budget.remaining -= Math.min(text.length, limit);
    if (text.length > limit) {
      budget.partial = true;
      return text.slice(0, limit) + "\n…";
    }
    return text;
  }
  if (depth >= 6) {
    budget.partial = true;
    return "[较深字段未展开]";
  }
  if (Array.isArray(v)) {
    budget.partial ||= v.length > 32;
    const result: Json[] = [];
    for (let i = 0; i < Math.min(32, v.length); i++) {
      if (budget.remaining <= 0) {
        budget.partial = true;
        break;
      }
      budget.remaining -= 8;
      result.push(observedValue(v[i], depth + 1, budget));
    }
    return result;
  }
  if (v && typeof v === "object") {
    const result: Record<string, Json> = Object.create(null);
    const keys = Object.keys(v);
    budget.partial ||= keys.length > 40;
    for (const key of keys.slice(0, 40)) {
      if (budget.remaining <= 0) {
        budget.partial = true;
        break;
      }
      budget.remaining -= key.length + 8;
      if (
        /^(authorization|apiKey|appSecret|password|token|privateKey|connectionFile|keyFile)$/i.test(
          key,
        )
      ) {
        result[key] = "[凭据字段不展示]";
      } else result[key] = observedValue((v as Record<string, unknown>)[key], depth + 1, budget);
    }
    return result;
  }
  return null;
}

/** A projection reports when any field was shortened or omitted. */
export function observedPreview(
  value: unknown,
  characters = 8192,
): { value: Json; partial: boolean } {
  const budget = { remaining: characters, partial: false };
  return { value: observedValue(value, 0, budget), partial: budget.partial };
}

/**
 * Observer never opens a writer or changes an index. It validates a bounded suffix of immutable
 * commit originals and their addressed objects; this is a recent activity view, not full recovery.
 */
export class ScriptObserver {
  #through = 0;
  #initialized = false;
  #cached: Json = { throughSeq: 0, items: [], partial: true };
  constructor(readonly directory: string) {}
  async read(): Promise<Json> {
    for (
      const path of [
        this.directory,
        `${this.directory}/commits`,
        `${this.directory}/commits/events`,
        `${this.directory}/objects`,
      ]
    ) {
      const stat = await Deno.lstat(path);
      if (!stat.isDirectory || stat.isSymlink) throw new KernelError("corrupt", "操作记录目录无效");
    }
    let through = this.#through;
    if (!this.#initialized) {
      for await (const file of Deno.readDir(`${this.directory}/commits/events`)) {
        if (file.isFile && /^\d{16}\.json$/.test(file.name)) {
          through = Math.max(through, Number(file.name.slice(0, 16)));
        }
      }
    } else {
      for (let n = 0; n < 256; n++) {
        try {
          await Deno.lstat(this.#eventPath(through + 1));
          through++;
        } catch (error) {
          if (!(error instanceof Deno.errors.NotFound)) throw error;
          break;
        }
      }
    }
    if (this.#initialized && through === this.#through) return this.#cached;
    let bytes = 0, objects = 0;
    const read = (r: Ref): unknown => {
      if (!ref(r)) throw new KernelError("corrupt", "操作原件引用无效");
      if (r.bytes > 4_194_304 || (bytes += r.bytes) > 33_554_432 || ++objects > 512) {
        throw new KernelError("unavailable", "近期操作原件超过只读窗口上限");
      }
      const path = `${this.directory}/objects/${r.hash}.json`;
      const stat = Deno.lstatSync(path);
      if (!stat.isFile || stat.isSymlink || stat.size !== r.bytes) {
        throw new KernelError("corrupt", "操作原件大小无效");
      }
      const value = JSON.parse(Deno.readTextFileSync(path));
      if (hash(value) !== r.hash) throw new KernelError("corrupt", "操作原件摘要不符");
      return value;
    };
    const decode = (encoded: unknown, depth = 0): unknown => {
      if (depth > 64) throw new KernelError("corrupt", "操作编码过深");
      if (!Array.isArray(encoded)) return encoded;
      if (encoded.length !== 2) throw new KernelError("corrupt", "操作编码无效");
      const [kind, value] = encoded;
      if (kind === "string" || kind === "node") {
        if (!ref(value)) throw new KernelError("corrupt", "操作引用无效");
        const saved = read(value);
        if (kind === "string") {
          if (typeof saved !== "string") throw new KernelError("corrupt", "操作字符串无效");
          return saved;
        }
        return decode(saved, depth + 1);
      }
      if (kind === "chunks") {
        const chunks = object(value);
        if (
          !Number.isSafeInteger(chunks.length) || Number(chunks.length) < 0 ||
          !Array.isArray(chunks.items) ||
          chunks.items.length !== Math.ceil(Number(chunks.length) / 256)
        ) {
          throw new KernelError("corrupt", "操作分块索引无效");
        }
        const items = chunks.items;
        let previous = -1, page: unknown[] = [];
        return new Proxy(new Array(Number(chunks.length)), {
          get(target, key, receiver) {
            if (typeof key !== "string" || !/^(0|[1-9]\d*)$/.test(key)) {
              return Reflect.get(target, key, receiver);
            }
            const index = Number(key);
            if (index >= target.length) return undefined;
            const chunk = Math.floor(index / 256);
            if (previous !== chunk) {
              const decoded = decode(items[chunk], depth + 1);
              if (
                !Array.isArray(decoded) ||
                decoded.length !== Math.min(256, target.length - chunk * 256)
              ) {
                throw new KernelError("corrupt", "操作分块长度无效");
              }
              previous = chunk;
              page = decoded;
            }
            return page[index % 256];
          },
          has(target, key) {
            return typeof key === "string" && /^(0|[1-9]\d*)$/.test(key)
              ? Number(key) < target.length
              : Reflect.has(target, key);
          },
        });
      }
      if (kind !== "object" && kind !== "array") {
        throw new KernelError("corrupt", "操作节点类型无效");
      }
      if (
        kind === "array"
          ? !Array.isArray(value)
          : !value || typeof value !== "object" || Array.isArray(value)
      ) {
        throw new KernelError("corrupt", "操作节点无效");
      }
      const result = kind === "array" ? [] : Object.create(null);
      for (const [key, child] of Object.entries(value)) {
        let loaded = false, saved: unknown;
        Object.defineProperty(result, key, {
          enumerable: true,
          get: () => {
            if (!loaded) {
              saved = decode(child, depth + 1);
              loaded = true;
            }
            return saved;
          },
        });
      }
      return result;
    };
    const jobs = new Map<number, { ref: Ref; sourceSeq: number }>();
    let jobsTruncated = false;
    let expectedDigest: string | undefined;
    for (let seq = through; seq >= Math.max(1, through - 127); seq--) {
      const path = this.#eventPath(seq), stat = await Deno.lstat(path);
      if (!stat.isFile || stat.isSymlink || stat.size > 1_048_576) {
        throw new KernelError("corrupt", "操作提交原件无效");
      }
      const event = JSON.parse(await Deno.readTextFile(path)), { digest, ...body } = event;
      if (
        event.schema !== 1 || event.seq !== seq || event.kind !== "script_commit" ||
        !/^[a-f0-9]{64}$/.test(event.previous) || hash(body) !== digest ||
        expectedDigest !== undefined && digest !== expectedDigest ||
        seq === 1 && event.previous !== "0".repeat(64)
      ) {
        throw new KernelError("corrupt", "操作提交摘要链不符");
      }
      expectedDigest = event.previous;
      if (event.data?.schema !== 2 || !ref(event.data.commit)) {
        throw new KernelError("corrupt", "操作提交信封无效");
      }
      const commit = object(decode(read(event.data.commit)));
      if (
        commit.schema !== 1 || !Array.isArray(commit.jobs) ||
        !Number.isSafeInteger(commit.jobsLength)
      ) {
        throw new KernelError("corrupt", "操作提交索引无效");
      }
      // Later commits win. Only retain the latest twelve changed jobs in this suffix.
      for (let index = commit.jobs.length - 1; index >= 0; index--) {
        const c = object(commit.jobs[index]);
        if (
          !Number.isSafeInteger(c.index) || Number(c.index) < 0 ||
          Number(c.index) >= Number(commit.jobsLength) || !ref(c.ref)
        ) {
          throw new KernelError("corrupt", "操作任务引用无效");
        }
        if (!jobs.has(Number(c.index))) {
          if (jobs.size < 12) jobs.set(Number(c.index), { ref: c.ref, sourceSeq: seq });
          else jobsTruncated = true;
        }
      }
    }
    const items: ObservedAction[] = [];
    let partial = through > 128 || jobsTruncated;
    for (const entry of jobs.values()) {
      try {
        const job = object(decode(read(entry.ref)));
        if (
          typeof job.jobId !== "string" || !/^[a-f0-9]{64}$/.test(job.jobId) ||
          !Array.isArray(job.actions)
        ) {
          throw new KernelError("corrupt", "操作任务原件无效");
        }
        for (const raw of job.actions.slice(-32)) {
          const action = object(raw),
            namespace = String(action.namespace),
            seq = Number(action.seq);
          if (
            !/^[a-zA-Z][a-zA-Z0-9_]{0,127}$/.test(namespace) ||
            typeof action.method !== "string" ||
            !/^[a-zA-Z][a-zA-Z0-9_]{0,127}$/.test(action.method) ||
            !Number.isSafeInteger(seq) || seq < 1 ||
            action.actionId !== `script_${job.jobId}_${seq}` ||
            !["started", "completed", "failed", "unknown"].includes(String(action.status)) ||
            !Number.isFinite(Date.parse(String(action.startedAt)))
          ) {
            throw new KernelError("corrupt", "具体操作原件无效");
          }
          const projected: ObservedAction = {
            id: String(action.actionId),
            jobId: job.jobId,
            namespace,
            method: String(action.method),
            status: String(action.status),
            at: String(action.finishedAt ?? action.startedAt),
            startedAt: String(action.startedAt),
            seq,
            sourceSeq: entry.sourceSeq,
            args: null,
            partial: false,
          };
          const budget: ViewBudget = { remaining: 12_000, partial: false };
          try {
            projected.args = observedValue(action.args, 0, budget);
            if (Object.hasOwn(action, "value")) {
              projected.result = observedValue(action.value, 0, budget);
            }
            if (Object.hasOwn(action, "error")) {
              projected.error = observedValue(action.error, 0, budget);
            }
          } catch (error) {
            if (!(error instanceof KernelError && error.kind === "unavailable")) throw error;
            projected.partial = true;
          }
          projected.partial ||= budget.partial;
          items.push(projected);
        }
        partial ||= job.actions.length > 32;
      } catch (error) {
        if (!(error instanceof KernelError && error.kind === "unavailable")) throw error;
        partial = true;
      }
    }
    items.sort((a, b) =>
      Date.parse(a.startedAt) - Date.parse(b.startedAt) || a.sourceSeq - b.sourceSeq ||
      a.seq - b.seq
    );
    const selected = items.slice(-64);
    const snapshot = {
      throughSeq: through,
      items: selected,
      partial: partial || items.length > 64 || selected.some((item) => item.partial),
      source: "script_ledger",
    };
    // IPC bounds encoded bytes, not JavaScript characters: CJK and escaped controls expand.
    // Preserve every action identity/status/source, shrinking only preview details as a group.
    if (new TextEncoder().encode(JSON.stringify(snapshot)).length > 750_000) {
      snapshot.items = selected.map((item) => {
        const budget: ViewBudget = { remaining: 512, partial: false };
        return {
          ...item,
          args: observedValue(item.args, 0, budget),
          ...(item.result === undefined ? {} : { result: observedValue(item.result, 0, budget) }),
          ...(item.error === undefined ? {} : { error: observedValue(item.error, 0, budget) }),
          partial: true,
        };
      });
      snapshot.partial = true;
    }
    this.#cached = snapshot as unknown as Json;
    this.#through = through;
    this.#initialized = true;
    return this.#cached;
  }
  #eventPath(seq: number) {
    return `${this.directory}/commits/events/${String(seq).padStart(16, "0")}.json`;
  }
}
