// 独立的事实账本，不参与插件快照回滚；每个脚本与嵌套动作独立持久提交。
import type { Json, WireError } from "../../kernel/protocol.ts";
import { KernelError } from "../../kernel/protocol.ts";
import type {
  ScriptCompletion,
  ScriptRequest,
  ScriptStatus,
  ScriptTiming,
} from "../../contracts/script.ts";
import { createHash } from "node:crypto";
import { canonical } from "../../kernel/calls.ts";
import { FactJournal } from "../../sdk/fact-journal.ts";
import { writeDurableJson } from "../../sdk/durable-json.ts";

export interface ActionRecord {
  seq: number;
  actionId: string;
  namespace: string;
  method: string;
  args: Json;
  status: "started" | "completed" | "failed" | "unknown";
  startedAt: string;
  finishedAt?: string;
  value?: Json;
  error?: WireError;
}
export interface JobRecord {
  jobId: string;
  fingerprint: string;
  request: ScriptRequest;
  status: ScriptStatus;
  scriptStarted: boolean;
  startedAt: string;
  finishedAt?: string;
  error?: WireError;
  result?: Json;
  timing?: ScriptTiming;
  actions: ActionRecord[];
}
export interface ScriptState {
  schema: 1;
  jobs: JobRecord[];
  completions: ScriptCompletion[];
}

interface ObjectRef {
  hash: string;
  bytes: number;
}
type Encoded =
  | null
  | boolean
  | number
  | string
  | ["array", Encoded[]]
  | [
    "object",
    Record<string, Encoded>,
  ]
  | ["string" | "node", ObjectRef]
  | ["chunks", { length: number; items: Encoded[] }];
interface Commit {
  schema: 1;
  jobsLength: number;
  completionsLength: number;
  jobs: { index: number; ref: ObjectRef }[];
  completions: { index: number; ref: ObjectRef }[];
  legacyDigest?: string;
}
const MARKER = { schema: 2, layout: "content-addressed-journal-v1" };
const OBJECT_LIMIT = 67_108_864;
const NODE_LIMIT = 65_536;
const STRING_LIMIT = 4096;
const hash = (value: Json) => createHash("sha256").update(canonical(value)).digest("hex");
function reference(value: unknown): value is ObjectRef {
  const ref = value as ObjectRef;
  return !!ref && typeof ref === "object" && /^[a-f0-9]{64}$/.test(ref.hash) &&
    Number.isSafeInteger(ref.bytes) && ref.bytes > 0 && ref.bytes <= OBJECT_LIMIT;
}
function numeric(property: PropertyKey) {
  if (typeof property !== "string" || !/^(0|[1-9]\d*)$/.test(property)) return;
  const index = Number(property);
  return Number.isSafeInteger(index) ? index : undefined;
}
/** 数组兼容的惰性视图；草稿只保存本轮访问的元数据，不解开大值引用。 */
function entries<T>(
  refs: ObjectRef[],
  read: (ref: ObjectRef, index: number) => T,
  mutable = false,
) {
  let length = refs.length;
  const loaded = new Map<number, T>();
  const view: T[] = new Proxy([] as T[], {
    get(target, property, receiver) {
      if (property === "length") return length;
      const index = numeric(property);
      if (index === undefined) return Reflect.get(target, property, receiver);
      if (index >= length) return undefined;
      if (loaded.has(index)) return loaded.get(index);
      if (!refs[index]) throw new KernelError("corrupt", "脚本账本数组存在空洞");
      const value = read(refs[index], index);
      if (mutable) loaded.set(index, value);
      return value;
    },
    has(target, property) {
      const index = numeric(property);
      return index === undefined ? Reflect.has(target, property) : index < length;
    },
    ownKeys() {
      return [...Array.from({ length }, (_, i) => String(i)), "length"];
    },
    getOwnPropertyDescriptor(target, property): PropertyDescriptor | undefined {
      if (property === "length") {
        return { ...Reflect.getOwnPropertyDescriptor(target, property)!, value: length };
      }
      const index = numeric(property);
      if (index === undefined || index >= length) return undefined;
      return { configurable: true, enumerable: true, writable: mutable, value: view[index] };
    },
    set(_target, property, value) {
      if (!mutable) throw new KernelError("rejected", "脚本账本视图只读，请使用 change");
      if (property === "length") {
        if (!Number.isSafeInteger(value) || value < 0 || value > 0xffff_ffff) {
          throw new KernelError("rejected", "脚本账本数组长度无效");
        }
        length = value;
        for (const index of loaded.keys()) if (index >= length) loaded.delete(index);
        return true;
      }
      const index = numeric(property);
      if (index === undefined) throw new KernelError("rejected", "脚本账本数组属性无效");
      loaded.set(index, value);
      length = Math.max(length, index + 1);
      return true;
    },
    deleteProperty(_target, property) {
      if (!mutable) throw new KernelError("rejected", "脚本账本视图只读");
      const index = numeric(property);
      if (index !== undefined) loaded.delete(index);
      return true;
    },
    defineProperty() {
      throw new KernelError("rejected", "脚本账本数组不能重定义属性");
    },
    preventExtensions() {
      throw new KernelError("rejected", "脚本账本数组不能封闭");
    },
    setPrototypeOf() {
      throw new KernelError("rejected", "脚本账本数组不能修改原型");
    },
  });
  return { view, loaded, length: () => length };
}

function sequence<T>(length: number, read: (index: number) => T): T[] {
  const view: T[] = new Proxy([] as T[], {
    get(target, property, receiver) {
      if (property === "length") return length;
      const index = numeric(property);
      return index === undefined
        ? Reflect.get(target, property, receiver)
        : index < length
        ? read(index)
        : undefined;
    },
    has(target, property) {
      const index = numeric(property);
      return index === undefined ? Reflect.has(target, property) : index < length;
    },
    ownKeys() {
      return [...Array.from({ length }, (_, i) => String(i)), "length"];
    },
    getOwnPropertyDescriptor(target, property): PropertyDescriptor | undefined {
      if (property === "length") {
        return { ...Reflect.getOwnPropertyDescriptor(target, property)!, value: length };
      }
      const index = numeric(property);
      return index !== undefined && index < length
        ? { configurable: true, enumerable: true, writable: false, value: read(index) }
        : undefined;
    },
    set() {
      throw new KernelError("rejected", "脚本原件数组只读");
    },
    deleteProperty() {
      throw new KernelError("rejected", "脚本原件数组只读");
    },
    defineProperty() {
      throw new KernelError("rejected", "脚本原件数组只读");
    },
    preventExtensions() {
      throw new KernelError("rejected", "脚本原件数组不能封闭");
    },
    setPrototypeOf() {
      throw new KernelError("rejected", "脚本原件数组不能修改原型");
    },
  });
  return view;
}

/** 不参与插件回滚。提交日志只存小引用，大值与元数据按内容寻址、去重、按需读取。 */
export class ScriptStore {
  #failed = false;
  #tail: Promise<unknown> = Promise.resolve();
  #lock?: Deno.FsFile;
  #closing = false;
  #closed = false;
  #initializing = false;
  #initializationFinished?: Promise<void>;
  #commits: FactJournal;
  #jobs: ObjectRef[] = [];
  #completions: ObjectRef[] = [];
  #jobLookup = new Map<string, number>();
  #jobIds: (string | undefined)[] = [];
  #fields = new WeakMap<object, Map<PropertyKey, Encoded>>();
  constructor(readonly directory: string) {
    this.#commits = new FactJournal(`${directory}/commits`);
  }
  get state(): ScriptState {
    return {
      schema: 1,
      jobs: entries(this.#jobs, (ref, index) => this.#readJob(ref, index, false)).view,
      completions:
        entries(this.#completions, (ref) => this.#readEntry<ScriptCompletion>(ref, false)).view,
    };
  }
  #readObject(ref: ObjectRef): Json {
    try {
      if (!reference(ref)) throw new Error("invalid ref");
      const path = `${this.directory}/objects/${ref.hash}.json`;
      const stat = Deno.lstatSync(path);
      if (!stat.isFile || stat.isSymlink || stat.size !== ref.bytes) {
        throw new Error("invalid object");
      }
      const value = JSON.parse(Deno.readTextFileSync(path));
      if (hash(value) !== ref.hash) throw new Error("changed object");
      return value;
    } catch {
      this.#failed = true;
      throw new KernelError("corrupt", "脚本账本内容原件缺失或摘要不符，原件保留");
    }
  }
  #decode(encoded: Encoded, mutable: boolean): Json {
    if (!Array.isArray(encoded)) {
      canonical(encoded);
      return encoded;
    }
    if (encoded.length !== 2) throw new KernelError("corrupt", "脚本账本编码损坏");
    const [kind, value] = encoded;
    if (kind === "string" || kind === "node") {
      if (!reference(value)) throw new KernelError("corrupt", "脚本账本大值引用损坏");
      const saved = this.#readObject(value);
      if (kind === "string") {
        if (typeof saved !== "string") throw new KernelError("corrupt", "脚本字符串原件类型错误");
        return saved;
      }
      return this.#decode(saved as Encoded, mutable);
    }
    if (kind === "chunks") {
      if (
        !value || typeof value !== "object" || !Number.isSafeInteger(value.length) ||
        value.length < 0 ||
        !Array.isArray(value.items) || value.items.length !== Math.ceil(value.length / 256)
      ) {
        throw new KernelError("corrupt", "脚本分块数组损坏");
      }
      let current = -1, chunk: Json[] = [];
      const view = sequence<Json>(value.length, (index) => {
        const number = Math.floor(index / 256);
        if (current !== number) {
          const decoded = this.#decode(value.items[number], mutable);
          if (
            !Array.isArray(decoded) || decoded.length !== Math.min(256, value.length - number * 256)
          ) {
            throw new KernelError("corrupt", "脚本数组分块长度不符");
          }
          current = number;
          chunk = decoded;
        }
        return chunk[index % 256];
      });
      return mutable ? Array.from(view) : view;
    }
    const result: Json[] | Record<string, Json> = kind === "array" ? [] : {};
    if (
      kind === "array"
        ? !Array.isArray(value)
        : kind !== "object" || !value || typeof value !== "object" || Array.isArray(value)
    ) {
      throw new KernelError("corrupt", "脚本账本元数据结构错误");
    }
    const fields = new Map<PropertyKey, Encoded>();
    this.#fields.set(result, fields);
    for (const [key, child] of Object.entries(value)) {
      if (Array.isArray(child) && ["string", "node", "chunks"].includes(child[0])) {
        fields.set(key, child as Encoded);
        Object.defineProperty(result, key, {
          enumerable: true,
          configurable: true,
          get: () => {
            const decoded = this.#decode(child as Encoded, mutable);
            if (mutable) {
              Object.defineProperty(result, key, {
                value: decoded,
                writable: true,
                enumerable: true,
                configurable: true,
              });
            }
            return decoded;
          },
          set: mutable
            ? (decoded) => {
              Object.defineProperty(result, key, {
                value: decoded,
                writable: true,
                enumerable: true,
                configurable: true,
              });
            }
            : undefined,
        });
      } else {
        Object.defineProperty(result, key, {
          value: this.#decode(child, mutable),
          writable: true,
          enumerable: true,
          configurable: true,
        });
      }
    }
    return result;
  }
  #readEntry<T>(ref: ObjectRef, mutable: boolean): T {
    return this.#decode(this.#readObject(ref) as Encoded, mutable) as unknown as T;
  }
  #readJob(ref: ObjectRef, index: number, mutable: boolean): JobRecord {
    const job = this.#readEntry<JobRecord>(ref, mutable);
    if (typeof job?.jobId === "string") {
      const previous = this.#jobIds[index];
      if (previous !== undefined && previous !== job.jobId) this.#jobLookup.delete(previous);
      this.#jobIds[index] = job.jobId;
      this.#jobLookup.set(job.jobId, index);
    }
    return job;
  }
  async #object(value: Json): Promise<ObjectRef> {
    const ref = {
      hash: hash(value),
      bytes: new TextEncoder().encode(JSON.stringify(value)).length + 1,
    };
    if (ref.bytes > OBJECT_LIMIT) throw new KernelError("storage", "脚本单份大值超出 64 MiB");
    const path = `${this.directory}/objects/${ref.hash}.json`;
    try {
      await Deno.lstat(path);
      this.#readObject(ref);
    } catch (error) {
      if (!(error instanceof Deno.errors.NotFound)) throw error;
      await writeDurableJson(path, value);
    }
    return ref;
  }
  async #encode(value: Json): Promise<Encoded> {
    if (typeof value === "string") {
      return value.length >= STRING_LIMIT ? ["string", await this.#object(value)] : value;
    }
    if (value === null || typeof value !== "object") {
      canonical(value);
      return value;
    }
    if (Array.isArray(value) && value.length > 256) {
      const items: Encoded[] = [];
      for (let start = 0; start < value.length; start += 256) {
        const chunk = await this.#encode(value.slice(start, start + 256));
        items.push(
          Array.isArray(chunk) && chunk[0] === "node"
            ? chunk
            : ["node", await this.#object(chunk as unknown as Json)],
        );
      }
      const encoded: Encoded = ["chunks", { length: value.length, items }];
      return new TextEncoder().encode(JSON.stringify(encoded)).length > NODE_LIMIT
        ? ["node", await this.#object(encoded as unknown as Json)]
        : encoded;
    }
    const fields = this.#fields.get(value);
    const children: Record<string, Encoded> | Encoded[] = Array.isArray(value) ? [] : {};
    for (const key of Object.keys(value)) {
      const descriptor = Object.getOwnPropertyDescriptor(value, key)!;
      const preserved = descriptor.get && fields?.get(key);
      const child = preserved || await this.#encode((value as Record<string, Json>)[key]);
      Object.defineProperty(children, key, {
        value: child,
        writable: true,
        configurable: true,
        enumerable: true,
      });
    }
    const encoded: Encoded = Array.isArray(value)
      ? ["array", children as Encoded[]]
      : ["object", children as Record<string, Encoded>];
    return new TextEncoder().encode(JSON.stringify(encoded)).length > NODE_LIMIT
      ? ["node", await this.#object(encoded as unknown as Json)]
      : encoded;
  }
  #apply(commit: Commit) {
    if (
      !commit || commit.schema !== 1 || !Number.isSafeInteger(commit.jobsLength) ||
      commit.jobsLength < 0 ||
      !Number.isSafeInteger(commit.completionsLength) || commit.completionsLength < 0 ||
      !Array.isArray(commit.jobs) || !Array.isArray(commit.completions)
    ) throw new KernelError("corrupt", "脚本提交索引损坏");
    for (
      const [refs, changes, length] of [[this.#jobs, commit.jobs, commit.jobsLength], [
        this.#completions,
        commit.completions,
        commit.completionsLength,
      ]] as const
    ) {
      const oldLength = refs.length;
      if (refs === this.#jobs) {
        for (let index = length; index < this.#jobIds.length; index++) {
          const id = this.#jobIds[index];
          if (id !== undefined) this.#jobLookup.delete(id);
        }
        this.#jobIds.length = length;
        for (const change of changes) {
          const id = this.#jobIds[change.index];
          if (id !== undefined) this.#jobLookup.delete(id);
          this.#jobIds[change.index] = undefined;
        }
      }
      refs.length = length;
      const touched = new Set<number>();
      for (const change of changes) {
        if (
          !change || !Number.isSafeInteger(change.index) || change.index < 0 ||
          change.index >= length ||
          touched.has(change.index) || !reference(change.ref)
        ) throw new KernelError("corrupt", "脚本提交引用损坏");
        touched.add(change.index);
        refs[change.index] = change.ref;
      }
      for (let index = oldLength; index < length; index++) {
        if (!refs[index]) throw new KernelError("corrupt", "脚本提交缺少原件引用");
      }
    }
  }
  async #commitState(state: ScriptState, legacyDigest?: string): Promise<void> {
    if (state.schema !== 1 || !Array.isArray(state.jobs) || !Array.isArray(state.completions)) {
      throw new KernelError("rejected", "脚本状态结构无效");
    }
    const commit: Commit = {
      schema: 1,
      jobsLength: state.jobs.length,
      completionsLength: state.completions.length,
      jobs: [],
      completions: [],
      ...(legacyDigest ? { legacyDigest } : {}),
    };
    for (
      const [values, previous, output] of [[state.jobs, this.#jobs, commit.jobs], [
        state.completions,
        this.#completions,
        commit.completions,
      ]] as const
    ) {
      for (let index = 0; index < values.length; index++) {
        const ref = await this.#object(
          await this.#encode(values[index] as unknown as Json) as unknown as Json,
        );
        if (previous[index]?.hash !== ref.hash || previous[index]?.bytes !== ref.bytes) {
          output.push({ index, ref });
        }
      }
    }
    const ref = await this.#object(
      await this.#encode(commit as unknown as Json) as unknown as Json,
    );
    await this.#commits.append("script_commit", { schema: 2, commit: ref } as unknown as Json);
    this.#apply(commit);
  }
  async initialize(): Promise<void> {
    if (this.#lock || this.#failed || this.#closing || this.#initializing) {
      throw new KernelError("unavailable", "脚本账本不可重复持有");
    }
    this.#initializing = true;
    let finished!: () => void;
    this.#initializationFinished = new Promise<void>((resolve) => {
      finished = resolve;
    });
    let lock: Deno.FsFile | undefined;
    try {
      await Deno.mkdir(this.directory, { recursive: true, mode: 0o700 });
      const directoryStat = await Deno.lstat(this.directory);
      if (!directoryStat.isDirectory || directoryStat.isSymlink) {
        throw new Error("invalid directory");
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
        throw new KernelError("unavailable", "已有脚本运行时持有账本");
      }
      for await (const file of Deno.readDir(this.directory)) {
        if (
          file.isSymlink ||
          !(file.isFile && ["writer.lock", "index.json", "legacy-index.json"].includes(file.name) ||
            file.isDirectory && ["commits", "objects"].includes(file.name))
        ) throw new KernelError("corrupt", "脚本账本有未完成或未知文件，原件保留");
      }
      await Deno.mkdir(`${this.directory}/objects`, { recursive: true, mode: 0o700 });
      const objectsStat = await Deno.lstat(`${this.directory}/objects`);
      if (!objectsStat.isDirectory || objectsStat.isSymlink) throw new Error("invalid objects");
      for await (const file of Deno.readDir(`${this.directory}/objects`)) {
        if (!file.isFile || file.isSymlink || !/^[a-f0-9]{64}\.json$/.test(file.name)) {
          throw new KernelError("corrupt", "脚本大值目录存在未完成原件，原件保留");
        }
      }
      await this.#commits.initialize();
      let marker: unknown;
      const indexPath = `${this.directory}/index.json`,
        legacyPath = `${this.directory}/legacy-index.json`;
      try {
        const stat = await Deno.lstat(indexPath);
        if (!stat.isFile || stat.isSymlink) throw new Error("invalid index");
        marker = JSON.parse(await Deno.readTextFile(indexPath));
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      let legacy: ScriptState | undefined;
      try {
        const stat = await Deno.lstat(legacyPath);
        if (!stat.isFile || stat.isSymlink) throw new Error("invalid legacy");
        legacy = JSON.parse(await Deno.readTextFile(legacyPath));
      } catch (error) {
        if (!(error instanceof Deno.errors.NotFound)) throw error;
      }
      if ((marker as ScriptState)?.schema === 1) {
        if (legacy || this.#commits.count) throw new Error("ambiguous legacy migration");
        legacy = marker as ScriptState;
        await Deno.rename(indexPath, legacyPath);
        const dir = await Deno.open(this.directory, { read: true });
        try {
          await dir.sync();
        } finally {
          dir.close();
        }
        marker = undefined;
      }
      if (marker !== undefined && canonical(marker as Json) !== canonical(MARKER)) {
        throw new Error("invalid marker");
      }
      if (
        legacy &&
        (legacy.schema !== 1 || !Array.isArray(legacy.jobs) || !Array.isArray(legacy.completions))
      ) throw new Error("invalid legacy");
      for (const event of this.#commits.events) {
        if (event.kind !== "script_commit") throw new Error("invalid commit kind");
        const envelope = event.data as unknown as { schema: number; commit: ObjectRef };
        if (envelope.schema !== 2 || !reference(envelope.commit)) {
          throw new Error("invalid commit envelope");
        }
        this.#apply(this.#readEntry<Commit>(envelope.commit, false));
      }
      if (marker === undefined) {
        if (legacy) {
          const digest = hash(legacy as unknown as Json);
          if (!this.#commits.count) await this.#commitState(legacy, digest);
          else if (
            this.#commits.count !== 1 ||
            this.#readEntry<Commit>(
                (this.#commits.events[0].data as unknown as { commit: ObjectRef }).commit,
                false,
              ).legacyDigest !== digest
          ) throw new Error("incomplete migration mismatch");
        } else if (this.#commits.count) throw new Error("missing marker");
        await writeDurableJson(indexPath, MARKER);
      }
      const legacyDigest = this.#commits.count
        ? this.#readEntry<Commit>(
          (this.#commits.events[0].data as unknown as { commit: ObjectRef }).commit,
          false,
        ).legacyDigest
        : undefined;
      if (
        legacyDigest !== undefined
          ? !legacy || legacyDigest !== hash(legacy as unknown as Json)
          : legacy !== undefined
      ) {
        throw new Error("legacy original missing or changed");
      }
      // 遍历并校验当前可达原件；不在内存保留正文，缺失引用不能启动成另一本账。
      const verified = new Set<string>();
      const verify = (ref: ObjectRef, stringOnly = false) => {
        if (verified.has(ref.hash)) return;
        const encoded = this.#readObject(ref);
        verified.add(ref.hash);
        if (stringOnly) {
          if (typeof encoded !== "string") throw new Error("invalid string");
          return;
        }
        const visit = (value: Encoded): void => {
          if (!Array.isArray(value)) {
            canonical(value);
            return;
          }
          if (value.length !== 2) throw new Error("invalid encoding");
          if (value[0] === "string" || value[0] === "node") {
            if (!reference(value[1])) throw new Error("invalid ref");
            verify(value[1], value[0] === "string");
          } else if (value[0] === "chunks") {
            const chunks = value[1];
            if (
              !chunks || !Number.isSafeInteger(chunks.length) || chunks.length < 0 ||
              !Array.isArray(chunks.items) || chunks.items.length !== Math.ceil(chunks.length / 256)
            ) throw new Error("invalid chunks");
            for (const child of chunks.items) visit(child);
          } else if (value[0] === "array" && Array.isArray(value[1])) {
            for (const child of value[1]) {
              visit(child);
            }
          } else if (
            value[0] === "object" && value[1] && typeof value[1] === "object" &&
            !Array.isArray(value[1])
          ) { for (const child of Object.values(value[1])) visit(child); } else {throw new Error(
              "invalid encoding",
            );}
        };
        visit(encoded as Encoded);
      };
      for (const ref of [...this.#jobs, ...this.#completions]) verify(ref);
      this.#lock = lock;
    } catch (error) {
      await this.#commits.close().catch(() => {});
      lock?.close();
      this.#failed = true;
      if (error instanceof KernelError) throw error;
      throw new KernelError("corrupt", "脚本账本读取失败，原件保留");
    } finally {
      this.#initializing = false;
      finished();
    }
  }
  get available() {
    return this.#lock !== undefined && !this.#failed && !this.#closed;
  }
  async change(edit: (state: ScriptState) => void): Promise<void> {
    if (this.#closing) throw new KernelError("storage", "脚本账本正在关闭");
    const operation = this.#tail.then(async () => {
      if (!this.available) throw new KernelError("storage", "脚本状态未保存，原件保留");
      const jobs = entries(this.#jobs, (ref, index) => this.#readJob(ref, index, true), true);
      const completions = entries(
        this.#completions,
        (ref) => this.#readEntry<ScriptCompletion>(ref, true),
        true,
      );
      const draft: ScriptState = { schema: 1, jobs: jobs.view, completions: completions.view };
      edit(draft);
      if (draft.schema !== 1 || !Array.isArray(draft.jobs) || !Array.isArray(draft.completions)) {
        throw new KernelError("rejected", "脚本状态结构无效");
      }
      try {
        const commit: Commit = {
          schema: 1,
          jobsLength: draft.jobs.length,
          completionsLength: draft.completions.length,
          jobs: [],
          completions: [],
        };
        for (
          const [values, draftArray, refs, changes] of [
            [draft.jobs, jobs, this.#jobs, commit.jobs],
            [draft.completions, completions, this.#completions, commit.completions],
          ] as const
        ) {
          const changed = values === draftArray.view
            ? draftArray.loaded
            : new Map<number, JobRecord | ScriptCompletion>(
              Array.from(values as (JobRecord | ScriptCompletion)[], (value, index) =>
                [index, value] as const),
            );
          for (const [index, value] of changed) {
            if (index >= values.length) continue;
            const ref = await this.#object(
              await this.#encode(value as unknown as Json) as unknown as Json,
            );
            if (refs[index]?.hash !== ref.hash || refs[index]?.bytes !== ref.bytes) {
              changes.push({ index, ref });
            }
          }
          for (let index = refs.length; index < values.length; index++) {
            if (!changed.has(index)) throw new Error("missing inserted entry");
          }
        }
        const ref = await this.#object(
          await this.#encode(commit as unknown as Json) as unknown as Json,
        );
        await this.#commits.append("script_commit", { schema: 2, commit: ref } as unknown as Json);
        this.#apply(commit);
      } catch (error) {
        this.#failed = true;
        if (error instanceof KernelError && error.kind === "corrupt") throw error;
        throw new KernelError("storage", "脚本状态未保存，原件保留");
      }
    });
    this.#tail = operation.catch(() => {});
    await operation;
  }
  async settle() {
    await this.#tail;
    if (this.#failed) throw new KernelError("storage", "脚本状态提交失败");
  }
  async close() {
    this.#closing = true;
    await this.#initializationFinished;
    await this.#tail;
    this.#closed = true;
    await this.#commits.close().catch((error) => {
      this.#failed = true;
      throw error;
    }).finally(() => {
      this.#lock?.close();
      this.#lock = undefined;
    });
    if (this.#failed) throw new KernelError("storage", "脚本账本关闭时有存盘失败");
  }
  /** 只索引小编号；普通遍历也会充填索引，不保留任务正文。 */
  jobIndex(jobId: string): number {
    if (!this.available) throw new KernelError("storage", "脚本状态不可用");
    const known = this.#jobLookup.get(jobId);
    if (known !== undefined) return known;
    for (let index = 0; index < this.#jobs.length; index++) {
      if (this.#jobIds[index] === undefined) this.#readJob(this.#jobs[index], index, false);
      if (this.#jobIds[index] === jobId) return index;
    }
    return -1;
  }
  findJob(jobId: string): JobRecord | undefined {
    const index = this.jobIndex(jobId);
    return index < 0 ? undefined : this.#readJob(this.#jobs[index], index, false);
  }
  job(jobId: string): JobRecord {
    const result = this.findJob(jobId);
    if (!result) throw new KernelError("not_found", "没有该脚本任务");
    return result;
  }
}
